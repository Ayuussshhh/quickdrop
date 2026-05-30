//! Self-contained HTML for the browser receiver.
//!
//! No external assets, fonts, or scripts — the page must render on a
//! phone with **no internet**, only LAN access to this server. All CSS
//! and JS are inlined.

use crate::session::PublicSession;

/// Minimal HTML-escape for text interpolated into the page.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// The download landing page for `GET /share/:id`.
pub fn landing_page(s: &PublicSession) -> String {
    let name = esc(&s.file_name);
    let size = human_size(s.file_size);
    let id = esc(&s.session_id);
    let pw_block = if s.password_protected {
        r#"<input id="pw" type="password" inputmode="text" placeholder="Password"
              autocomplete="off" class="pw" />"#
    } else {
        ""
    };
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover" />
<meta name="color-scheme" content="light dark" />
<title>{name} · QuickDrop Share</title>
<style>
  :root {{ --bg:#0f172a; --card:#ffffff; --fg:#0f172a; --muted:#64748b; --accent:#2563eb; }}
  @media (prefers-color-scheme: dark) {{
    :root {{ --bg:#020617; --card:#0f172a; --fg:#e2e8f0; --muted:#94a3b8; --accent:#3b82f6; }}
  }}
  * {{ box-sizing:border-box; }}
  body {{ margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center;
         background:var(--bg); color:var(--fg); font:16px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif; padding:20px; }}
  .card {{ background:var(--card); border-radius:20px; padding:28px 24px; width:100%; max-width:380px;
          box-shadow:0 20px 50px rgba(0,0,0,.35); text-align:center; }}
  .logo {{ font-weight:700; letter-spacing:.5px; color:var(--muted); font-size:13px; text-transform:uppercase; }}
  .name {{ font-size:20px; font-weight:650; margin:14px 0 4px; word-break:break-word; }}
  .size {{ color:var(--muted); font-size:14px; }}
  .pw {{ width:100%; margin-top:18px; padding:12px 14px; border-radius:12px; border:1px solid #cbd5e1;
        font-size:16px; background:transparent; color:var(--fg); }}
  .btn {{ display:block; width:100%; margin-top:18px; padding:14px; border:0; border-radius:14px;
         background:var(--accent); color:#fff; font-size:16px; font-weight:600; text-decoration:none; cursor:pointer; }}
  .btn:active {{ transform:translateY(1px); }}
  .err {{ color:#ef4444; font-size:13px; margin-top:12px; min-height:16px; }}
  .foot {{ color:var(--muted); font-size:12px; margin-top:18px; }}
</style>
</head>
<body>
  <div class="card">
    <div class="logo">QuickDrop Share</div>
    <div class="name">{name}</div>
    <div class="size">{size}</div>
    {pw_block}
    <button id="dl" class="btn">Download</button>
    <div id="err" class="err"></div>
    <div class="foot">Private LAN transfer · link expires automatically</div>
  </div>
<script>
  (function() {{
    var id = {id_json};
    var btn = document.getElementById('dl');
    var err = document.getElementById('err');
    var pw = document.getElementById('pw');
    btn.addEventListener('click', function() {{
      err.textContent = '';
      var url = '/download/' + encodeURIComponent(id);
      if (pw) url += '?pw=' + encodeURIComponent(pw.value || '');
      // Navigating triggers the browser's native download manager,
      // which handles huge files and resume far better than fetch().
      window.location.href = url;
    }});
  }})();
</script>
</body>
</html>"##,
        name = name,
        size = size,
        pw_block = pw_block,
        id_json = serde_json::to_string(&id).unwrap_or_else(|_| "\"\"".into()),
    )
}

/// Friendly 404 page (also used for expired/revoked, by design).
pub fn not_found_page() -> String {
    r##"<!doctype html><html lang="en"><head><meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>Link expired · QuickDrop Share</title>
<style>body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;
background:#0f172a;color:#e2e8f0;font:16px system-ui,sans-serif;text-align:center;padding:20px}
.c{max-width:360px}h1{font-size:20px;margin:0 0 8px}p{color:#94a3b8}</style></head>
<body><div class="c"><h1>This link is no longer available</h1>
<p>The share may have expired, reached its download limit, or been stopped by the sender.</p>
</div></body></html>"##
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_file_names() {
        let s = PublicSession {
            session_id: "abc".into(),
            file_name: "<script>x</script>.txt".into(),
            file_size: 2048,
            expires_at: 0,
            password_protected: false,
        };
        let html = landing_page(&s);
        assert!(!html.contains("<script>x</script>.txt"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("2.0 KB"));
    }

    #[test]
    fn password_field_only_when_protected() {
        let mut s = PublicSession {
            session_id: "abc".into(),
            file_name: "f.bin".into(),
            file_size: 1,
            expires_at: 0,
            password_protected: false,
        };
        assert!(!landing_page(&s).contains("id=\"pw\""));
        s.password_protected = true;
        assert!(landing_page(&s).contains("id=\"pw\""));
    }
}
