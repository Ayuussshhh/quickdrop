//! QuickDrop Spaces — foundation.
//!
//! A **Space** is a named container that groups *members* (paired
//! devices) and *shared folders*, with an append-only *activity feed*.
//! Spaces are the foundation for future collaborative sync; this module
//! deliberately implements only the durable data model, storage layout,
//! and the local activity log. It does **not** implement collaboration,
//! comments, chat, or any network sync — those land on top of this
//! foundation later.
//!
//! ## Storage layout
//!
//! Two sled trees:
//!
//! * `spaces/meta/v1` — one JSON [`Space`] per id (members + folders are
//!   embedded; the set is small and always read whole).
//! * `spaces/activity/v1` — the activity feed, keyed by
//!   `space_id (16) || timestamp_ms (8 BE) || seq`, so a space's events
//!   are a contiguous, chronologically-ordered key range.
//!
//! ## Sync architecture (design, not yet implemented)
//!
//! Every [`Space`] carries a monotonically increasing [`Space::revision`]
//! and an `updated_at_ms`. Mutations bump both. The activity feed is an
//! ordered, immutable log. Together these give future sync a clean basis:
//!
//! * compare `revision`/`updated_at_ms` to detect divergence cheaply;
//! * exchange the activity-log tail past a known sequence number to
//!   converge (a CRDT-style, last-writer-wins merge on metadata plus an
//!   ordered union of activity entries by their unique id);
//! * because activity ids are UUIDs and entries are immutable, merges are
//!   idempotent and order-independent.
//!
//! Nothing here talks to the network yet — the shapes are simply chosen
//! so that adding a sync engine later requires no migration.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::Db;
use crate::{Error, Result};

const TREE_SPACES: &str = "spaces/meta/v1";
const TREE_ACTIVITY: &str = "spaces/activity/v1";

/// The kind of a Space. Drives UI grouping today and may drive
/// default membership/permission policy in the future.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SpaceType {
    #[default]
    Personal,
    Project,
    Family,
    Team,
}

/// A member's role within a Space. Permissions are not enforced yet;
/// the field exists so future collaboration can build on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    #[default]
    Owner,
    Editor,
    Viewer,
}

/// A device that belongs to a Space.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    /// Stable device id (matches a discovery/trust peer id).
    pub peer_id: Uuid,
    pub name: String,
    pub role: MemberRole,
    pub joined_at_ms: u64,
}

/// A folder shared into a Space. Stored as a local path on the device
/// that owns it; future sync resolves these per-member.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedFolder {
    pub id: Uuid,
    pub name: String,
    pub path: PathBuf,
    pub added_by: Uuid,
    pub added_at_ms: u64,
}

/// The category of an activity-feed entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    SpaceCreated,
    MemberAdded,
    MemberRemoved,
    FolderAdded,
    FolderRemoved,
}

/// One immutable entry in a Space's activity feed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub id: Uuid,
    pub space_id: Uuid,
    pub kind: ActivityKind,
    /// Device that performed the action, if known.
    pub actor: Option<Uuid>,
    /// Human-readable detail, e.g. the member or folder name.
    pub detail: String,
    pub timestamp_ms: u64,
}

/// A Space: members + shared folders + sync metadata. The activity feed
/// is stored separately and fetched on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Space {
    pub id: Uuid,
    pub name: String,
    pub space_type: SpaceType,
    pub created_at_ms: u64,
    /// Bumped on every mutation; basis for future divergence detection.
    pub revision: u64,
    /// Wall-clock millis of the last mutation.
    pub updated_at_ms: u64,
    pub members: Vec<Member>,
    pub shared_folders: Vec<SharedFolder>,
}

/// Durable store for Spaces and their activity feeds.
#[derive(Debug, Clone)]
pub struct SpaceStore {
    spaces: sled::Tree,
    activity: sled::Tree,
}

impl SpaceStore {
    pub fn open(db: &Db) -> Result<Self> {
        let spaces = db.inner.open_tree(TREE_SPACES)?;
        let activity = db.inner.open_tree(TREE_ACTIVITY)?;
        Ok(Self { spaces, activity })
    }

    /// Create a new Space owned by `owner` (the local device). Logs a
    /// `SpaceCreated` activity entry.
    pub fn create(
        &self,
        name: String,
        space_type: SpaceType,
        owner: Uuid,
        owner_name: String,
    ) -> Result<Space> {
        let now = now_ms();
        let id = Uuid::new_v4();
        let space = Space {
            id,
            name: name.clone(),
            space_type,
            created_at_ms: now,
            revision: 1,
            updated_at_ms: now,
            members: vec![Member {
                peer_id: owner,
                name: owner_name,
                role: MemberRole::Owner,
                joined_at_ms: now,
            }],
            shared_folders: Vec::new(),
        };
        self.put(&space)?;
        self.log(id, ActivityKind::SpaceCreated, Some(owner), name)?;
        Ok(space)
    }

    pub fn get(&self, id: Uuid) -> Result<Option<Space>> {
        match self.spaces.get(id.as_bytes())? {
            Some(v) => Ok(Some(serde_json::from_slice(&v)?)),
            None => Ok(None),
        }
    }

    /// All spaces, newest first.
    pub fn list(&self) -> Result<Vec<Space>> {
        let mut out = Vec::new();
        for kv in self.spaces.iter() {
            let (_, v) = kv?;
            out.push(serde_json::from_slice::<Space>(&v)?);
        }
        out.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms));
        Ok(out)
    }

    pub fn delete(&self, id: Uuid) -> Result<bool> {
        let existed = self.spaces.remove(id.as_bytes())?.is_some();
        if existed {
            // Drop the space's activity range too.
            let prefix = id.as_bytes().to_vec();
            let keys: Vec<sled::IVec> = self
                .activity
                .scan_prefix(&prefix)
                .keys()
                .collect::<std::result::Result<_, _>>()?;
            for k in keys {
                self.activity.remove(k)?;
            }
            self.spaces.flush()?;
            self.activity.flush()?;
        }
        Ok(existed)
    }

    /// Add a member. Returns the updated space, or an error if the
    /// space does not exist. Idempotent on `peer_id`.
    pub fn add_member(
        &self,
        space_id: Uuid,
        peer_id: Uuid,
        name: String,
        role: MemberRole,
    ) -> Result<Space> {
        let mut space = self.require(space_id)?;
        if !space.members.iter().any(|m| m.peer_id == peer_id) {
            space.members.push(Member {
                peer_id,
                name: name.clone(),
                role,
                joined_at_ms: now_ms(),
            });
            self.bump(&mut space);
            self.put(&space)?;
            self.log(space_id, ActivityKind::MemberAdded, Some(peer_id), name)?;
        }
        Ok(space)
    }

    pub fn remove_member(&self, space_id: Uuid, peer_id: Uuid) -> Result<Space> {
        let mut space = self.require(space_id)?;
        let before = space.members.len();
        let name = space
            .members
            .iter()
            .find(|m| m.peer_id == peer_id)
            .map(|m| m.name.clone())
            .unwrap_or_default();
        space.members.retain(|m| m.peer_id != peer_id);
        if space.members.len() != before {
            self.bump(&mut space);
            self.put(&space)?;
            self.log(space_id, ActivityKind::MemberRemoved, Some(peer_id), name)?;
        }
        Ok(space)
    }

    /// Add a shared folder owned by `added_by`.
    pub fn add_folder(
        &self,
        space_id: Uuid,
        name: String,
        path: PathBuf,
        added_by: Uuid,
    ) -> Result<Space> {
        let mut space = self.require(space_id)?;
        space.shared_folders.push(SharedFolder {
            id: Uuid::new_v4(),
            name: name.clone(),
            path,
            added_by,
            added_at_ms: now_ms(),
        });
        self.bump(&mut space);
        self.put(&space)?;
        self.log(space_id, ActivityKind::FolderAdded, Some(added_by), name)?;
        Ok(space)
    }

    pub fn remove_folder(&self, space_id: Uuid, folder_id: Uuid) -> Result<Space> {
        let mut space = self.require(space_id)?;
        let before = space.shared_folders.len();
        let name = space
            .shared_folders
            .iter()
            .find(|f| f.id == folder_id)
            .map(|f| f.name.clone())
            .unwrap_or_default();
        space.shared_folders.retain(|f| f.id != folder_id);
        if space.shared_folders.len() != before {
            self.bump(&mut space);
            self.put(&space)?;
            self.log(space_id, ActivityKind::FolderRemoved, None, name)?;
        }
        Ok(space)
    }

    /// Return a space's activity feed, newest first.
    pub fn activity(&self, space_id: Uuid) -> Result<Vec<Activity>> {
        let mut out = Vec::new();
        for kv in self.activity.scan_prefix(space_id.as_bytes()) {
            let (_, v) = kv?;
            out.push(serde_json::from_slice::<Activity>(&v)?);
        }
        out.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
        Ok(out)
    }

    // --- internals -----------------------------------------------------

    fn require(&self, id: Uuid) -> Result<Space> {
        self.get(id)?
            .ok_or_else(|| Error::Internal(format!("space {id} not found")))
    }

    fn put(&self, space: &Space) -> Result<()> {
        let bytes = serde_json::to_vec(space)?;
        self.spaces.insert(space.id.as_bytes(), bytes)?;
        self.spaces.flush()?;
        Ok(())
    }

    fn bump(&self, space: &mut Space) {
        space.revision = space.revision.saturating_add(1);
        space.updated_at_ms = now_ms();
    }

    fn log(
        &self,
        space_id: Uuid,
        kind: ActivityKind,
        actor: Option<Uuid>,
        detail: String,
    ) -> Result<()> {
        let now = now_ms();
        let entry = Activity {
            id: Uuid::new_v4(),
            space_id,
            kind,
            actor,
            detail,
            timestamp_ms: now,
        };
        // Key: space_id || timestamp_ms (BE) || activity id — keeps a
        // space's feed contiguous and chronologically ordered while
        // staying unique within a millisecond.
        let mut key = Vec::with_capacity(16 + 8 + 16);
        key.extend_from_slice(space_id.as_bytes());
        key.extend_from_slice(&now.to_be_bytes());
        key.extend_from_slice(entry.id.as_bytes());
        let bytes = serde_json::to_vec(&entry)?;
        self.activity.insert(key, bytes)?;
        self.activity.flush()?;
        Ok(())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, SpaceStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let store = SpaceStore::open(&db).unwrap();
        (dir, store)
    }

    #[test]
    fn create_adds_owner_and_activity() {
        let (_d, s) = temp();
        let owner = Uuid::new_v4();
        let sp = s
            .create("Family".into(), SpaceType::Family, owner, "Me".into())
            .unwrap();
        assert_eq!(sp.members.len(), 1);
        assert_eq!(sp.members[0].role, MemberRole::Owner);
        assert_eq!(sp.revision, 1);
        let feed = s.activity(sp.id).unwrap();
        assert_eq!(feed.len(), 1);
        assert_eq!(feed[0].kind, ActivityKind::SpaceCreated);
    }

    #[test]
    fn members_folders_bump_revision_and_log() {
        let (_d, s) = temp();
        let owner = Uuid::new_v4();
        let sp = s
            .create("P".into(), SpaceType::Project, owner, "Me".into())
            .unwrap();
        let peer = Uuid::new_v4();
        let sp = s
            .add_member(sp.id, peer, "Bob".into(), MemberRole::Editor)
            .unwrap();
        assert_eq!(sp.members.len(), 2);
        assert!(sp.revision >= 2);
        let sp = s
            .add_folder(sp.id, "Docs".into(), PathBuf::from("/docs"), owner)
            .unwrap();
        assert_eq!(sp.shared_folders.len(), 1);
        // create + member + folder = 3 entries.
        assert_eq!(s.activity(sp.id).unwrap().len(), 3);
    }

    #[test]
    fn delete_removes_space_and_feed() {
        let (_d, s) = temp();
        let sp = s
            .create("X".into(), SpaceType::Personal, Uuid::new_v4(), "Me".into())
            .unwrap();
        assert!(s.delete(sp.id).unwrap());
        assert!(s.get(sp.id).unwrap().is_none());
        assert!(s.activity(sp.id).unwrap().is_empty());
    }
}
