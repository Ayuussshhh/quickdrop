import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import "./App.css";

type AppInfo = {
  version: string;
  device_name: string;
  device_id: string;
  fingerprint: string;
  destination: string;
  app_data: string;
};

type Peer = {
  id: string;
  name: string;
  os: string;
  device_type: string;
  trusted: boolean;
  addrs: string[];
  fingerprint: string;
};

type TrustedPeer = {
  id: string;
  name: string;
  fingerprint: string;
  paired_at_ms: number;
  last_seen_ms: number;
};

type TransferProgress = {
  transfer_id: string;
  direction: string;
  peer_name: string;
  peer_id: string;
  completed_items: number;
  total_items: number;
  bytes_done: number;
  total_bytes: number;
  speed_bps: number;
  state: string;
  note: string;
  started_at_ms: number;
};

type Prompt = {
  prompt_id: string;
  kind: "pair" | "transfer";
  peer_id: string;
  peer_name: string;
  fingerprint: string;
  sas?: string;
  items?: number;
  total_bytes?: number;
  trusted: boolean;
};

type Tab = "devices" | "transfers" | "trusted" | "share";

type ShareSession = {
  session_id: string;
  file_name: string;
  file_size: number;
  created_at: number;
  expires_at: number;
  download_count: number;
  max_downloads: number;
  file_path: string;
  password_protected: boolean;
};

type TicketUrl = {
  url: string;
  label: string;
  is_hostname: boolean;
};

type ShareTicket = {
  session: ShareSession;
  url: string;
  urls: TicketUrl[];
  qr_svg: string;
  qr_terminal: string;
};

function fmtBytes(n: number): string {
  if (!n || n < 0) return "0 B";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${u[i]}`;
}

function fmtRate(bps: number): string {
  return `${fmtBytes(bps)}/s`;
}

function App() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [peers, setPeers] = useState<Peer[]>([]);
  const [trusted, setTrusted] = useState<TrustedPeer[]>([]);
  const [transfers, setTransfers] = useState<TransferProgress[]>([]);
  const [prompts, setPrompts] = useState<Prompt[]>([]);
  const [outgoingSas, setOutgoingSas] = useState<{ peer_id: string; sas: string } | null>(null);
  const [tab, setTab] = useState<Tab>("devices");
  const [sendTarget, setSendTarget] = useState<Peer | null>(null);
  const [sendPaths, setSendPaths] = useState<string[]>([]);
  const [shareTicket, setShareTicket] = useState<ShareTicket | null>(null);
  const [shares, setShares] = useState<ShareSession[]>([]);
  const [sharing, setSharing] = useState(false);
  const [toast, setToast] = useState<string | null>(null);

  async function refreshTrusted() {
    try {
      setTrusted(await invoke<TrustedPeer[]>("list_trusted_peers"));
    } catch (e) {
      console.error("list_trusted_peers failed", e);
    }
  }

  useEffect(() => {
    invoke<AppInfo>("app_info").then(setInfo).catch(console.error);
    invoke<Peer[]>("list_peers").then(setPeers).catch(console.error);
    invoke<TransferProgress[]>("list_transfers").then(setTransfers).catch(console.error);
    refreshTrusted();

    const unlisteners: Array<Promise<() => void>> = [];

    unlisteners.push(
      listen<Peer[]>("peers://updated", (e) => {
        setPeers(e.payload);
      })
    );
    unlisteners.push(
      listen<TransferProgress[]>("transfers://updated", (e) => {
        setTransfers(e.payload);
      })
    );
    unlisteners.push(
      listen<string>("transfers://error", (e) => {
        setToast(`Send failed: ${e.payload}`);
        setTimeout(() => setToast(null), 4000);
      })
    );
    unlisteners.push(
      listen<string[]>("transfers://received", (e) => {
        setToast(`Received ${e.payload.length} file(s)`);
        setTimeout(() => setToast(null), 4000);
        refreshTrusted();
      })
    );
    unlisteners.push(
      listen<Prompt>("prompt://incoming", (e) => {
        setPrompts((p) => [...p, e.payload]);
      })
    );
    unlisteners.push(
      listen<{ peer_id: string; sas: string }>("pairing://sas", (e) => {
        setOutgoingSas(e.payload);
      })
    );
    unlisteners.push(
      listen<string>("pairing://done", () => {
        setOutgoingSas(null);
        refreshTrusted();
        setToast("Paired successfully");
        setTimeout(() => setToast(null), 3000);
      })
    );
    unlisteners.push(
      listen<string[]>("send://files", (e) => {
        setSendPaths(e.payload);
        setTab("devices");
      })
    );

    return () => {
      unlisteners.forEach((p) => p.then((u) => u()));
    };
  }, []);

  async function handleForget(id: string) {
    try {
      await invoke<boolean>("forget_peer", { peerId: id });
      await refreshTrusted();
    } catch (e) {
      console.error("forget_peer failed", e);
    }
  }

  async function handlePair(peer: Peer) {
    try {
      await invoke("pair_with", { peerId: peer.id });
    } catch (e) {
      setToast(`Pair failed: ${e}`);
      setTimeout(() => setToast(null), 4000);
      setOutgoingSas(null);
    }
  }

  async function handleStartSend(peer: Peer) {
    if (sendPaths.length === 0) {
      const picked = await openDialog({ multiple: true, directory: false });
      if (!picked) return;
      const arr = Array.isArray(picked) ? picked : [picked];
      setSendPaths(arr as string[]);
    }
    setSendTarget(peer);
  }

  async function confirmSend() {
    if (!sendTarget || sendPaths.length === 0) return;
    try {
      await invoke<string>("send_files", {
        peerId: sendTarget.id,
        paths: sendPaths,
      });
      setTab("transfers");
    } catch (e) {
      setToast(`Send failed: ${e}`);
      setTimeout(() => setToast(null), 4000);
    } finally {
      setSendTarget(null);
      setSendPaths([]);
    }
  }

  async function answerPrompt(prompt_id: string, accept: boolean) {
    try {
      await invoke("answer_prompt", { promptId: prompt_id, accept });
    } catch (e) {
      console.error("answer_prompt failed", e);
    }
    setPrompts((p) => p.filter((x) => x.prompt_id !== prompt_id));
  }

  async function cancelTransfer(id: string) {
    try {
      await invoke<boolean>("cancel_transfer", { transferId: id });
    } catch (e) {
      console.error(e);
    }
  }

  async function pickFiles() {
    const picked = await openDialog({ multiple: true, directory: false });
    if (!picked) return;
    setSendPaths((Array.isArray(picked) ? picked : [picked]) as string[]);
  }

  async function refreshShares() {
    try {
      setShares(await invoke<ShareSession[]>("share_list"));
    } catch (e) {
      console.error("share_list failed", e);
    }
  }

  async function startShare() {
    const picked = await openDialog({ multiple: false, directory: false });
    if (!picked || Array.isArray(picked)) return;
    setSharing(true);
    try {
      const ticket = await invoke<ShareTicket>("share_file", {
        path: picked,
        ttlSecs: 30 * 60,
        maxDownloads: 0,
      });
      setShareTicket(ticket);
      await refreshShares();
    } catch (e) {
      setToast(`Share failed: ${e}`);
      setTimeout(() => setToast(null), 4000);
    } finally {
      setSharing(false);
    }
  }

  async function stopShare(sessionId: string) {
    try {
      await invoke<boolean>("share_stop", { sessionId });
    } catch (e) {
      console.error("share_stop failed", e);
    }
    if (shareTicket?.session.session_id === sessionId) setShareTicket(null);
    await refreshShares();
  }

  // Poll live shares while the Share tab or the QR modal is visible so
  // the download counter and expiry stay fresh.
  useEffect(() => {
    if (tab !== "share" && !shareTicket) return;
    refreshShares();
    const h = setInterval(refreshShares, 2000);
    return () => clearInterval(h);
  }, [tab, shareTicket]);

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="brand-mark" />
          <span className="brand-name">QuickDrop</span>
          {info && <span className="brand-version">v{info.version}</span>}
        </div>
        {info && (
          <div className="me">
            <span className="me-label">This device</span>
            <span className="me-name">{info.device_name}</span>
            <span className="me-fp" title={`Device ID: ${info.device_id}`}>
              {info.fingerprint}
            </span>
          </div>
        )}
      </header>

      <nav className="tabs">
        <TabButton active={tab === "devices"} onClick={() => setTab("devices")}>
          Devices ({peers.length})
        </TabButton>
        <TabButton active={tab === "transfers"} onClick={() => setTab("transfers")}>
          Transfers (
          {transfers.filter((t) => t.state === "Active" || t.state === "Pending").length}
          )
        </TabButton>
        <TabButton active={tab === "trusted"} onClick={() => setTab("trusted")}>
          Trusted ({trusted.length})
        </TabButton>
        <TabButton active={tab === "share"} onClick={() => setTab("share")}>
          Share ({shares.length})
        </TabButton>
      </nav>

      <main className="content">
        {tab === "devices" && (
          <DevicesPane
            peers={peers}
            sendPaths={sendPaths}
            onPickFiles={pickFiles}
            onClearPaths={() => setSendPaths([])}
            onSend={handleStartSend}
            onPair={handlePair}
          />
        )}
        {tab === "transfers" && (
          <TransfersPane transfers={transfers} onCancel={cancelTransfer} />
        )}
        {tab === "trusted" && <TrustedPane peers={trusted} onForget={handleForget} />}
        {tab === "share" && (
          <SharePane
            shares={shares}
            sharing={sharing}
            onStartShare={startShare}
            onStopShare={stopShare}
          />
        )}
      </main>

      {info && (
        <footer className="statusbar">
          <span>Receiving to: {info.destination}</span>
          <span className="dot">•</span>
          <span>{info.fingerprint}</span>
        </footer>
      )}

      {sendTarget && (
        <Modal title={`Send to ${sendTarget.name}`} onClose={() => setSendTarget(null)}>
          <p className="modal-sub">{sendPaths.length} file(s) selected</p>
          <ul className="path-list">
            {sendPaths.slice(0, 50).map((p) => (
              <li key={p}>{p}</li>
            ))}
            {sendPaths.length > 50 && <li>… and {sendPaths.length - 50} more</li>}
          </ul>
          <div className="modal-actions">
            <button className="btn" onClick={() => setSendTarget(null)}>
              Cancel
            </button>
            <button className="btn btn-primary" onClick={confirmSend}>
              Send
            </button>
          </div>
        </Modal>
      )}

      {outgoingSas && (
        <Modal title="Pairing — verify code" onClose={() => setOutgoingSas(null)}>
          <p className="modal-sub">Make sure this matches the code on the other device:</p>
          <div className="sas">{outgoingSas.sas}</div>
          <p className="modal-hint">Waiting for the other device to confirm…</p>
        </Modal>
      )}

      {prompts.map((p) => (
        <PromptModal key={p.prompt_id} prompt={p} onAnswer={answerPrompt} />
      ))}

      {shareTicket && (
        <ShareModal
          ticket={shareTicket}
          live={shares.find((s) => s.session_id === shareTicket.session.session_id)}
          onClose={() => setShareTicket(null)}
          onStop={() => stopShare(shareTicket.session.session_id)}
          onToast={(m) => {
            setToast(m);
            setTimeout(() => setToast(null), 2500);
          }}
        />
      )}

      {toast && <div className="toast">{toast}</div>}
    </div>
  );
}

function TabButton({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button className={`tab ${active ? "tab-active" : ""}`} onClick={onClick}>
      {children}
    </button>
  );
}

function DevicesPane({
  peers,
  sendPaths,
  onPickFiles,
  onClearPaths,
  onSend,
  onPair,
}: {
  peers: Peer[];
  sendPaths: string[];
  onPickFiles: () => void;
  onClearPaths: () => void;
  onSend: (p: Peer) => void;
  onPair: (p: Peer) => void;
}) {
  return (
    <div>
      <div className="send-bar">
        {sendPaths.length === 0 ? (
          <button className="btn" onClick={onPickFiles}>
            Choose files to send…
          </button>
        ) : (
          <>
            <span>{sendPaths.length} file(s) ready — pick a device</span>
            <button className="btn-link" onClick={onClearPaths}>
              Clear
            </button>
          </>
        )}
      </div>
      {peers.length === 0 ? (
        <EmptyState
          title="No devices found yet"
          body="Devices on the same Wi-Fi network will appear here automatically."
        />
      ) : (
        <ul className="device-list">
          {peers.map((p) => (
            <li key={p.id} className="device">
              <div className="device-main">
                <div className="device-name">{p.name}</div>
                <div className="device-meta">
                  {p.device_type} · {p.os} · {p.fingerprint}{" "}
                  {p.trusted && <span className="badge">trusted</span>}
                </div>
              </div>
              <div className="device-actions">
                <button
                  className="btn btn-primary"
                  onClick={() => onSend(p)}
                  disabled={sendPaths.length === 0}
                  title={sendPaths.length === 0 ? "Choose files first" : ""}
                >
                  Send
                </button>
                {!p.trusted && (
                  <button className="btn" onClick={() => onPair(p)}>
                    Pair
                  </button>
                )}
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function TransfersPane({
  transfers,
  onCancel,
}: {
  transfers: TransferProgress[];
  onCancel: (id: string) => void;
}) {
  const sorted = useMemo(
    () => [...transfers].sort((a, b) => b.started_at_ms - a.started_at_ms),
    [transfers]
  );
  if (sorted.length === 0) {
    return <EmptyState title="No transfers" body="Sends and receives will appear here." />;
  }
  return (
    <ul className="xfer-list">
      {sorted.map((t) => {
        const pct =
          t.total_bytes > 0 ? Math.min(100, (t.bytes_done / t.total_bytes) * 100) : 0;
        const active = t.state === "Active" || t.state === "Pending";
        return (
          <li key={t.transfer_id} className={`xfer xfer-${t.state.toLowerCase()}`}>
            <div className="xfer-head">
              <span className="xfer-dir">{t.direction === "Send" ? "↑" : "↓"}</span>
              <span className="xfer-peer">{t.peer_name}</span>
              <span className="xfer-state">{t.state}</span>
              {active && (
                <button className="btn-link" onClick={() => onCancel(t.transfer_id)}>
                  Cancel
                </button>
              )}
            </div>
            <div className="xfer-bar">
              <div style={{ width: `${pct}%` }} />
            </div>
            <div className="xfer-meta">
              {fmtBytes(t.bytes_done)} / {fmtBytes(t.total_bytes)}
              {active && t.speed_bps > 0 && <> · {fmtRate(t.speed_bps)}</>}
              {t.note && <> · {t.note}</>}
            </div>
          </li>
        );
      })}
    </ul>
  );
}

function TrustedPane({
  peers,
  onForget,
}: {
  peers: TrustedPeer[];
  onForget: (id: string) => void;
}) {
  if (peers.length === 0) {
    return (
      <EmptyState
        title="No trusted devices yet"
        body="When you pair with another device, it will appear here."
      />
    );
  }
  return (
    <ul className="trusted-list">
      {peers.map((p) => (
        <li key={p.id} className="trusted">
          <div className="trusted-main">
            <div className="trusted-name">{p.name}</div>
            <div className="trusted-fp">{p.fingerprint}</div>
            <div className="trusted-meta">
              Paired {new Date(p.paired_at_ms).toLocaleString()}
              {p.last_seen_ms > p.paired_at_ms && (
                <> · last seen {new Date(p.last_seen_ms).toLocaleString()}</>
              )}
            </div>
          </div>
          <button className="btn-danger" onClick={() => onForget(p.id)}>
            Forget
          </button>
        </li>
      ))}
    </ul>
  );
}

function SharePane({
  shares,
  sharing,
  onStartShare,
  onStopShare,
}: {
  shares: ShareSession[];
  sharing: boolean;
  onStartShare: () => void;
  onStopShare: (id: string) => void;
}) {
  return (
    <div>
      <div className="send-bar">
        <button className="btn btn-primary" onClick={onStartShare} disabled={sharing}>
          {sharing ? "Starting…" : "Share a file to phone…"}
        </button>
        <span className="modal-hint">
          Receiver only needs the same Wi-Fi and a browser — no app, no account.
        </span>
      </div>
      {shares.length === 0 ? (
        <EmptyState
          title="Nothing shared right now"
          body="Share a file to get a QR code any phone can scan to download it."
        />
      ) : (
        <ul className="device-list">
          {shares.map((s) => (
            <li key={s.session_id} className="device">
              <div className="device-main">
                <div className="device-name">{s.file_name}</div>
                <div className="device-meta">
                  {fmtBytes(s.file_size)} · {s.download_count}
                  {s.max_downloads > 0 ? `/${s.max_downloads}` : ""} download(s) ·{" "}
                  <Countdown expiresAt={s.expires_at} />
                  {s.password_protected && <span className="badge">password</span>}
                </div>
              </div>
              <div className="device-actions">
                <button className="btn-danger" onClick={() => onStopShare(s.session_id)}>
                  Stop
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function ShareModal({
  ticket,
  live,
  onClose,
  onStop,
  onToast,
}: {
  ticket: ShareTicket;
  live?: ShareSession;
  onClose: () => void;
  onStop: () => void;
  onToast: (m: string) => void;
}) {
  const s = live ?? ticket.session;
  const downloads = s.download_count;
  return (
    <Modal title="Share to phone" onClose={onClose}>
      <p className="modal-sub">{ticket.session.file_name}</p>
      <div
        style={{
          display: "flex",
          justifyContent: "center",
          padding: 16,
          background: "#fff",
          borderRadius: 12,
          margin: "8px auto",
          maxWidth: 260,
        }}
        // The SVG is generated locally by our own server (qrcode crate),
        // not from any remote or user-controlled source.
        dangerouslySetInnerHTML={{ __html: ticket.qr_svg }}
      />
      <p className="modal-hint" style={{ fontSize: 11, opacity: 0.6, marginBottom: 0 }}>
        QR encodes: {ticket.url}
      </p>
      <p className="modal-hint">Scan QR with phone camera, or open one of the URLs below.</p>
      {ticket.urls.map((entry) => (
        <div
          key={entry.url}
          style={{
            display: "flex",
            alignItems: "center",
            gap: 10,
            background: entry.is_hostname ? "rgba(0,0,0,0.15)" : "rgba(99,102,241,0.15)",
            border: entry.is_hostname ? "1px solid rgba(255,255,255,0.1)" : "1px solid rgba(99,102,241,0.4)",
            borderRadius: 8,
            padding: "8px 12px",
            marginTop: 6,
            opacity: entry.is_hostname ? 0.55 : 1,
          }}
        >
          <div style={{ flex: 1, minWidth: 0 }}>
            <div style={{ fontSize: 10, fontWeight: 600, textTransform: "uppercase", letterSpacing: "0.05em", opacity: 0.7, marginBottom: 2 }}>
              {entry.label}{entry.is_hostname ? " — may not work on Android" : ""}
            </div>
            <code style={{ fontSize: 12, wordBreak: "break-all", display: "block" }}>{entry.url}</code>
          </div>
          <button
            className="btn-link"
            onClick={async () => {
              try { await navigator.clipboard.writeText(entry.url); onToast("Copied"); }
              catch { onToast("Copy failed"); }
            }}
          >
            Copy
          </button>
        </div>
      ))}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 8,
          justifyContent: "center",
          fontSize: 12,
          marginTop: 12,
          opacity: 0.8,
        }}
      >
        <span>
          {downloads}
          {s.max_downloads > 0 ? `/${s.max_downloads}` : ""} download(s)
        </span>
        <span className="dot">•</span>
        <span>
          Expires in <Countdown expiresAt={s.expires_at} />
        </span>
      </div>
      <div className="modal-actions">
        <button className="btn" onClick={onClose}>
          Close
        </button>
        <button className="btn-danger" onClick={onStop}>
          Stop sharing
        </button>
      </div>
    </Modal>
  );
}

function Countdown({ expiresAt }: { expiresAt: number }) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const h = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(h);
  }, []);
  const ms = Math.max(0, expiresAt - now);
  if (ms === 0) return <span>expired</span>;
  const total = Math.floor(ms / 1000);
  const m = Math.floor(total / 60);
  const sec = total % 60;
  return (
    <span>
      {m}:{sec.toString().padStart(2, "0")}
    </span>
  );
}

function EmptyState({ title, body }: { title: string; body: string }) {
  return (
    <div className="empty">
      <h2>{title}</h2>
      <p>{body}</p>
    </div>
  );
}

function Modal({
  title,
  children,
  onClose,
}: {
  title: string;
  children: React.ReactNode;
  onClose: () => void;
}) {
  return (
    <div className="modal-back" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>{title}</h2>
        {children}
      </div>
    </div>
  );
}

function PromptModal({
  prompt,
  onAnswer,
}: {
  prompt: Prompt;
  onAnswer: (id: string, accept: boolean) => void;
}) {
  return (
    <Modal
      title={prompt.kind === "pair" ? "Incoming pairing request" : "Incoming file transfer"}
      onClose={() => onAnswer(prompt.prompt_id, false)}
    >
      <p className="modal-sub">
        From <strong>{prompt.peer_name}</strong>{" "}
        {prompt.trusted && <span className="badge">trusted</span>}
      </p>
      <p className="modal-fp">Fingerprint: {prompt.fingerprint}</p>
      {prompt.kind === "pair" && (
        <>
          <p>Verify this code matches the one shown on the other device:</p>
          <div className="sas">{prompt.sas}</div>
        </>
      )}
      {prompt.kind === "transfer" && (
        <p>
          {prompt.items} item(s), {fmtBytes(prompt.total_bytes ?? 0)} total
        </p>
      )}
      <div className="modal-actions">
        <button className="btn" onClick={() => onAnswer(prompt.prompt_id, false)}>
          Reject
        </button>
        <button className="btn btn-primary" onClick={() => onAnswer(prompt.prompt_id, true)}>
          Accept
        </button>
      </div>
    </Modal>
  );
}

export default App;
