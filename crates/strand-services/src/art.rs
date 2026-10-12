//! Remote album art for `media.art`: an `https://` or `http://`
//! `mpris:artUrl` (Spotify's, a browser's) fetched and kept as a file, so
//! `image media.art` shows it by path like local art.
//!
//! A fetch runs on the runtime's blocking pool, never on the media body's
//! loop, the logic thread or a frame: the bytes are read at most
//! [`MAX_BYTES`] within [`TIMEOUT`] (redirects included), checked to be a
//! picture `image` decodes (PNG, JPEG, GIF or WebP, by its signature) and
//! written as a content-addressed file next to the notifications' and
//! tray's pictures ([`crate::pixmap`]); decoding stays with the renderer,
//! which decodes at the drawn size off the render thread. TLS is rustls
//! (ring) trusting the system's certificates (`SSL_CERT_FILE` and
//! `SSL_CERT_DIR` are honoured); no OpenSSL. A proxy in `https_proxy`,
//! `http_proxy` or `all_proxy` is used, minus `no_proxy`.
//!
//! [`Cache`] keeps the last [`CACHED`] URLs' files, at most [`KEPT_BYTES`]
//! of them together (the oldest shown least recently is dropped, and its
//! file with its last handle; the one shown last never is): the files are
//! on `$XDG_RUNTIME_DIR`, a tmpfs, so they are memory outside the
//! process's own, and the bound keeps it to a few covers. It runs at most
//! [`IN_FLIGHT`] fetches at once, and remembers failures: a URL that
//! failed is not asked again for [`RETRY_AFTER`], however often a player
//! repeats it, so a broken URL costs one request, not a storm. While a
//! URL is fetched, or after it failed, `art` is null and a shell's
//! fallback (`media.art ?? "audio-x-generic"`) shows.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::pixmap::{self, Pinned};

/// The largest picture fetched, bytes (a 3000 px JPEG cover is about
/// 1–2 MB).
pub const MAX_BYTES: u64 = 8 << 20;
/// How long one fetch may take, connecting, redirects and body included.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Redirects followed.
pub const REDIRECTS: u32 = 4;
/// URLs whose files are kept.
pub const CACHED: usize = 16;
/// The kept files' bytes together (on a tmpfs: memory): two of the
/// largest pictures, or many covers of the usual 100 KB–2 MB.
pub const KEPT_BYTES: u64 = 2 * MAX_BYTES;
/// Fetches at once.
pub const IN_FLIGHT: usize = 2;
/// Failed URLs remembered.
pub const FAILED: usize = 64;
/// How long a failed URL is not asked again.
pub const RETRY_AFTER: Duration = Duration::from_secs(300);

/// Whether `url` is remote art this fetches (`https://` or `http://`,
/// any case).
pub fn is_remote(url: &str) -> bool {
    let lower = |n: usize| url.get(..n).map(str::to_ascii_lowercase);
    lower(8).as_deref() == Some("https://") || lower(7).as_deref() == Some("http://")
}

/// The file extension of the picture `bytes` hold, by signature, if it is
/// one `image` decodes.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// An HTTP client trusting `roots` (DER certificates).
pub fn agent(roots: Vec<Vec<u8>>) -> ureq::Agent {
    use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};
    let roots: Vec<Certificate<'static>> = roots
        .iter()
        .map(|der| Certificate::from_der(der).to_owned())
        .collect();
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .root_certs(RootCerts::from(roots))
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(TIMEOUT))
        .max_redirects(REDIRECTS)
        .user_agent("strand")
        .accept("image/*")
        .build()
        .new_agent()
}

/// The system's client: its certificates read once, on the first fetch
/// (on the blocking pool).
fn system() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let found = rustls_native_certs::load_native_certs();
        for e in &found.errors {
            log::debug!("art: system certificates: {e}");
        }
        if found.certs.is_empty() {
            log::warn!("art: no system certificates found: https art will not load");
        }
        agent(found.certs.iter().map(|c| c.as_ref().to_vec()).collect())
    })
}

/// A fetched picture: its file and its size.
#[derive(Debug, PartialEq)]
pub struct Art {
    /// The file, kept while this lives.
    pub file: Pinned,
    /// Its bytes.
    pub bytes: u64,
}

/// Fetch `url` with the system's client: blocking (call it on a blocking
/// pool), bounded by [`TIMEOUT`] and [`MAX_BYTES`].
pub fn fetch(url: &str) -> Result<Art, String> {
    fetch_with(system(), url)
}

/// Fetch `url` with `agent` and keep it as a file; or why not.
pub fn fetch_with(agent: &ureq::Agent, url: &str) -> Result<Art, String> {
    if !is_remote(url) {
        return Err(format!("{url}: not an http(s) URL"));
    }
    let mut response = agent.get(url).call().map_err(|e| e.to_string())?;
    let body = response.body_mut();
    if body.content_length().is_some_and(|n| n > MAX_BYTES) {
        return Err(format!("larger than {MAX_BYTES} bytes"));
    }
    let bytes = body
        .with_config()
        .limit(MAX_BYTES)
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    let ext = sniff(&bytes).ok_or("not a PNG, JPEG, GIF or WebP picture")?;
    let file = pixmap::write_file(&bytes, ext)?;
    Ok(Art {
        file,
        bytes: bytes.len() as u64,
    })
}

/// Remote art fetched, being fetched and failed, by URL (see the module
/// docs).
#[derive(Debug, Default)]
pub struct Cache {
    /// Fetched: the least recently shown first.
    kept: VecDeque<(String, Art)>,
    /// Their bytes together.
    bytes: u64,
    /// The URL shown last (never dropped to make room).
    shown: Option<String>,
    /// Being fetched.
    fetching: HashSet<String>,
    /// Failed, and when.
    failed: VecDeque<(String, Instant)>,
}

impl Cache {
    /// The file to show for `url`, if it is fetched (it becomes the most
    /// recently shown).
    pub fn file(&mut self, url: &str) -> Option<String> {
        let i = self.kept.iter().position(|(u, _)| u == url)?;
        let entry = self.kept.remove(i)?;
        let text = entry.1.file.text();
        self.kept.push_back(entry);
        self.shown = Some(url.to_string());
        Some(text)
    }

    /// Whether `url` should be fetched now: not kept, not being fetched,
    /// not failed within [`RETRY_AFTER`], and fewer than [`IN_FLIGHT`]
    /// fetches running. If so it counts as being fetched from now.
    pub fn start(&mut self, url: &str, now: Instant) -> bool {
        if self.kept.iter().any(|(u, _)| u == url)
            || self.fetching.contains(url)
            || self.fetching.len() >= IN_FLIGHT
        {
            return false;
        }
        self.failed
            .retain(|(_, at)| now.saturating_duration_since(*at) < RETRY_AFTER);
        if self.failed.iter().any(|(u, _)| u == url) {
            return false;
        }
        self.fetching.insert(url.to_string());
        true
    }

    /// A fetch of `url` ended at `now`.
    pub fn done(&mut self, url: String, result: Result<Art, String>, now: Instant) {
        self.fetching.remove(&url);
        match result {
            Ok(art) => {
                self.drop_kept(|u| u == url);
                self.bytes += art.bytes;
                self.kept.push_back((url, art));
                // Within both bounds, dropping the least recently shown
                // first, but neither the newest nor the one shown last.
                while self.kept.len() > CACHED || self.bytes > KEPT_BYTES {
                    let newest = self.kept.len() - 1;
                    let shown = self.shown.as_ref();
                    let Some(i) = self.kept.iter().position(|(u, _)| Some(u) != shown) else {
                        break;
                    };
                    if i == newest {
                        break;
                    }
                    if let Some((_, gone)) = self.kept.remove(i) {
                        self.bytes -= gone.bytes;
                    }
                }
            }
            Err(e) => {
                log::info!("media: art {url} not loaded: {e}");
                self.failed.retain(|(u, _)| *u != url);
                if self.failed.len() == FAILED {
                    self.failed.pop_front();
                }
                self.failed.push_back((url, now));
            }
        }
    }

    /// Drop the kept URLs `which` names.
    fn drop_kept(&mut self, which: impl Fn(&str) -> bool) {
        let bytes = &mut self.bytes;
        self.kept.retain(|(u, art)| {
            let keep = !which(u);
            if !keep {
                *bytes -= art.bytes;
            }
            keep
        });
    }

    /// Fetches running.
    pub fn in_flight(&self) -> usize {
        self.fetching.len()
    }

    /// URLs kept.
    pub fn kept(&self) -> usize {
        self.kept.len()
    }

    /// The kept files' bytes together.
    pub fn kept_bytes(&self) -> u64 {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A tiny PNG (1x1).
    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 1, 1);
            enc.set_color(png::ColorType::Rgba);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 255]).unwrap();
        }
        out
    }

    #[test]
    fn only_http_urls_are_remote_and_only_pictures_pass() {
        assert!(is_remote("https://i.scdn.co/image/ab67"));
        assert!(is_remote("HTTP://x/a.jpg"));
        assert!(!is_remote("file:///tmp/a.png"));
        assert!(!is_remote("/tmp/a.png"));
        assert!(!is_remote("ftp://x/a.png"));
        assert!(!is_remote("http"));
        assert_eq!(sniff(&png()), Some("png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some("jpg"));
        assert_eq!(sniff(b"GIF89a...."), Some("gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(sniff(b"<!doctype html><html>"), None);
        assert_eq!(sniff(b"<svg xmlns="), None, "remote SVG is not taken");
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn the_cache_is_bounded_and_does_not_ask_a_failed_url_again() {
        let t0 = Instant::now();
        let mut c = Cache::default();
        let file = || Art {
            file: pixmap::write_png(&png()).unwrap(),
            bytes: 70,
        };
        assert!(c.start("https://a/1", t0));
        assert!(!c.start("https://a/1", t0), "already being fetched");
        assert!(c.start("https://a/2", t0));
        assert!(!c.start("https://a/3", t0), "two at once at most");
        c.done("https://a/1".into(), Ok(file()), t0);
        assert!(c.file("https://a/1").is_some());
        assert!(!c.start("https://a/1", t0), "kept");
        // A failure: not asked again until RETRY_AFTER has passed.
        c.done("https://a/2".into(), Err("404".into()), t0);
        assert_eq!(c.file("https://a/2"), None);
        assert!(!c.start("https://a/2", t0 + Duration::from_secs(1)));
        assert!(c.start("https://a/2", t0 + RETRY_AFTER));
        c.done("https://a/2".into(), Err("404".into()), t0 + RETRY_AFTER);
        // Bounded: the least recently shown goes first.
        for i in 0..CACHED + 4 {
            let u = format!("https://b/{i}");
            assert!(c.start(&u, t0));
            c.done(u, Ok(file()), t0);
            // a/1 is shown all along: it stays.
            assert!(c.file("https://a/1").is_some(), "{i}");
        }
        assert_eq!(c.kept(), CACHED);
        assert!(c.file("https://b/0").is_none(), "the oldest went");
        assert!(c.file(&format!("https://b/{}", CACHED + 3)).is_some());
        assert_eq!(c.in_flight(), 0);
        let mut f = Cache::default();
        for i in 0..FAILED + 10 {
            let u = format!("https://f/{i}");
            assert!(f.start(&u, t0));
            f.done(u, Err("gone".into()), t0);
        }
        assert_eq!(f.failed.len(), FAILED, "failures bounded too");
    }

    /// (m6-services, services-r1-3) The kept files are bounded by their
    /// bytes too (they are tmpfs memory): large covers push the least
    /// recently shown out, but never the one shown last or the newest.
    #[test]
    fn the_cache_is_bounded_by_its_bytes_and_keeps_what_is_shown() {
        let t0 = Instant::now();
        let mut c = Cache::default();
        // Distinct files (content-addressed), claimed to be large.
        let big = |i: u8| Art {
            file: pixmap::write_file(&[0x89, b'P', b'N', b'G', i, 1], "png").unwrap(),
            bytes: MAX_BYTES / 2 + 1,
        };
        let add = |c: &mut Cache, u: &str, i: u8| {
            assert!(c.start(u, t0));
            c.done(u.into(), Ok(big(i)), t0);
        };
        add(&mut c, "https://a/0", 0);
        assert!(c.file("https://a/0").is_some());
        for i in 1..8u8 {
            add(&mut c, &format!("https://a/{i}"), i);
            assert!(c.kept_bytes() <= KEPT_BYTES, "{i}: {}", c.kept_bytes());
            assert!(c.file("https://a/0").is_some(), "{i}: the shown one stays");
            assert!(
                c.file(&format!("https://a/{i}")).is_some(),
                "{i}: the newest stays"
            );
            // a/i was shown last now; show a/0 again.
            assert!(c.file("https://a/0").is_some());
        }
        assert_eq!(c.kept(), 3, "three of these fit in {KEPT_BYTES} bytes");
        assert!(
            c.file("https://a/1").is_none(),
            "the least recently shown went"
        );
        // Never below the two it must keep, however large.
        let mut d = Cache::default();
        let huge = |i: u8| Art {
            file: pixmap::write_file(&[0x89, b'P', b'N', b'G', i, 2], "png").unwrap(),
            bytes: KEPT_BYTES,
        };
        assert!(d.start("https://h/0", t0));
        d.done("https://h/0".into(), Ok(huge(0)), t0);
        assert!(d.file("https://h/0").is_some());
        assert!(d.start("https://h/1", t0));
        d.done("https://h/1".into(), Ok(huge(1)), t0);
        assert_eq!(d.kept(), 2);
        assert!(d.start("https://h/2", t0));
        d.done("https://h/2".into(), Ok(huge(2)), t0);
        assert_eq!(d.kept(), 2);
        assert!(d.file("https://h/0").is_some() && d.file("https://h/2").is_some());
        assert_eq!(d.kept_bytes(), 2 * KEPT_BYTES);
    }

    /// A one-thread HTTP/1.1 server answering every request with `reply`
    /// (status line, headers, body) and closing; its port and how many
    /// requests it got.
    fn serve(
        tls: Option<Arc<rustls::ServerConfig>>,
        reply: impl Fn(&str) -> Vec<u8> + Send + 'static,
    ) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                h.fetch_add(1, Ordering::SeqCst);
                let answer = |io: &mut dyn ReadWrite| {
                    let mut reader = BufReader::new(&mut *io);
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                    loop {
                        let mut l = String::new();
                        match reader.read_line(&mut l) {
                            Ok(0) | Err(_) => return,
                            Ok(_) if l == "\r\n" => break,
                            Ok(_) => {}
                        }
                    }
                    drop(reader);
                    let _ = io.write_all(&reply(&path));
                    let _ = io.flush();
                };
                match &tls {
                    Some(cfg) => {
                        let Ok(conn) = rustls::ServerConnection::new(cfg.clone()) else {
                            continue;
                        };
                        let mut s = rustls::StreamOwned::new(conn, stream);
                        answer(&mut s);
                        s.conn.send_close_notify();
                        let _ = s.flush();
                    }
                    None => {
                        let mut s = stream;
                        answer(&mut s);
                    }
                }
            }
        });
        (port, hits)
    }

    trait ReadWrite: std::io::Read + std::io::Write {}
    impl<T: std::io::Read + std::io::Write> ReadWrite for T {}

    fn ok(body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    /// A 200 with `body` in chunks of 64 KiB and no `Content-Length`.
    fn chunked(body: &[u8]) -> Vec<u8> {
        let mut r = b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for c in body.chunks(64 << 10) {
            r.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
            r.extend_from_slice(c);
            r.extend_from_slice(b"\r\n");
        }
        r.extend_from_slice(b"0\r\n\r\n");
        r
    }

    /// The test CA and the server's certificate and key (P-256, valid
    /// until 2126, for `localhost` and 127.0.0.1), made with openssl:
    /// `ecparam -genkey`, a self-signed CA:TRUE root, and a CA:FALSE leaf
    /// with those subjectAltNames, as DER.
    fn tls() -> (Vec<u8>, Arc<rustls::ServerConfig>) {
        const CA: &[u8] = include_bytes!("../tests/fixtures/art-tls/ca.der");
        const CERT: &[u8] = include_bytes!("../tests/fixtures/art-tls/server.der");
        const KEY: &[u8] = include_bytes!("../tests/fixtures/art-tls/server.key.der");
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(CERT.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
        )
        .unwrap();
        (CA.to_vec(), Arc::new(cfg))
    }

    #[test]
    fn https_art_is_fetched_checked_and_kept_as_a_file() {
        let (ca, cfg) = tls();
        let picture = png();
        let p = picture.clone();
        let (port, hits) = serve(Some(cfg), move |path| {
            match path {
            "/cover.png" => ok(&p),
            "/page" => ok(b"<!doctype html><title>not a picture</title>"),
            "/moved" => b"HTTP/1.1 302 Found\r\nLocation: /cover.png\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            "/chunked-huge" => chunked(&{
                let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
                b.resize((MAX_BYTES + (1 << 20)) as usize, 0);
                b
            }),
            "/chunked-small" => chunked(&p),
            "/huge" => format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_BYTES + 1
            )
            .into_bytes(),
            _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        }
        });
        let trusted = agent(vec![ca]);
        let url = |p: &str| format!("https://localhost:{port}{p}");
        let art = fetch_with(&trusted, &url("/cover.png")).unwrap();
        assert_eq!(art.bytes, picture.len() as u64);
        let file = art.file;
        assert_eq!(std::fs::read(file.path()).unwrap(), picture);
        assert!(file.text().ends_with(".png"), "{}", file.text());
        // Redirects are followed.
        let again = fetch_with(&trusted, &url("/moved")).unwrap().file;
        assert_eq!(again, file, "the same picture is the same file");
        // Not a picture, too large, missing: refused, no file.
        let e = fetch_with(&trusted, &url("/page")).unwrap_err();
        assert!(e.contains("not a PNG"), "{e}");
        let e = fetch_with(&trusted, &url("/huge")).unwrap_err();
        assert!(e.contains("larger than"), "{e}");
        assert!(fetch_with(&trusted, &url("/gone")).is_err());
        // (services-r1-7) A picture sent with no Content-Length (chunked)
        // is cut off at MAX_BYTES as it is read: 1 MiB over is refused,
        // though it is under ureq's own default limit.
        let e = fetch_with(&trusted, &url("/chunked-huge")).unwrap_err();
        assert!(!e.is_empty());
        let n = fetch_with(&trusted, &url("/chunked-small")).unwrap().bytes;
        assert_eq!(
            n,
            picture.len() as u64,
            "a chunked picture within it is taken"
        );
        // A server no trusted root vouches for is refused.
        let before = hits.load(Ordering::SeqCst);
        let untrusted = agent(Vec::new());
        assert!(fetch_with(&untrusted, &url("/cover.png")).is_err());
        assert_eq!(hits.load(Ordering::SeqCst), before + 1, "it connected");
    }

    #[test]
    fn plain_http_art_is_fetched_and_a_stalled_server_times_out() {
        let picture = png();
        let p = picture.clone();
        let (port, _) = serve(None, move |_| ok(&p));
        let file = fetch_with(
            &agent(Vec::new()),
            &format!("http://127.0.0.1:{port}/a.png"),
        )
        .unwrap()
        .file;
        assert_eq!(std::fs::read(file.path()).unwrap(), picture);
        // A server that accepts and never answers: the fetch gives up.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _held = std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().take(1).collect();
            std::thread::sleep(TIMEOUT + Duration::from_secs(5));
            drop(held);
        });
        let asked = Instant::now();
        assert!(
            fetch_with(
                &agent(Vec::new()),
                &format!("http://127.0.0.1:{port}/a.png")
            )
            .is_err()
        );
        let took = asked.elapsed();
        assert!(
            took >= TIMEOUT - Duration::from_millis(100) && took < TIMEOUT + Duration::from_secs(3),
            "{took:?}"
        );
    }
}
