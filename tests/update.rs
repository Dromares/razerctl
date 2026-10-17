//! The in-app update check and the AppImage update (DAN-1669), against a local HTTP fixture; nothing here touches the network.
//!
//! The release JSON has the shape of the authenticated `GET /repos/Dromares/razerctl/releases/tags/v0.2.0` response (probed
//! 2026-10-17: 200 with a token, 404 anonymously because the repo is private): the same keys and nesting, with the asset names
//! the release workflow publishes (`razerctl-<ver>-x86_64.AppImage`, `SHA256SUMS`).

use razerctl::sha256;
use razerctl::update::{
    apply_appimage, check, check_and_record, check_if_due, dismiss, pending_release, Applied, CheckResult, FetchError,
    InstallType, Outcome, Paths, Source, Stage, UpdateError, Version, INTERVAL_SECS,
};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

// ------------------------------------------------------------------------------------------------ fixture server

#[derive(Clone)]
struct Route {
    status: u16,
    body: Vec<u8>,
}

struct Fixture {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
    routes: Arc<Mutex<HashMap<String, Route>>>,
}

impl Fixture {
    fn start() -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let (h, r) = (hits.clone(), routes.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let (h, r) = (h.clone(), r.clone());
                std::thread::spawn(move || {
                    let mut s = stream.unwrap();
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let line = String::from_utf8_lossy(&req);
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    h.lock().unwrap().push(path.clone());
                    let route = r.lock().unwrap().get(&path).cloned();
                    let route = route.unwrap_or(Route { status: 404, body: b"{\"message\":\"Not Found\"}".to_vec() });
                    let head = format!(
                        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        route.status,
                        route.body.len()
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(&route.body);
                });
            }
        });
        Fixture { base, hits, routes }
    }

    fn src(&self) -> Source {
        Source::loopback(&self.base)
    }

    fn put(&self, path: &str, status: u16, body: impl Into<Vec<u8>>) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.into(), Route { status, body: body.into() });
    }

    fn requests(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }

    fn count(&self, path: &str) -> usize {
        self.requests().iter().filter(|p| *p == path).count()
    }

    /// Publishes `tag` as the latest release with the AppImage `payload` and a `SHA256SUMS` that lists `sums_for` (normally the
    /// payload itself; a different byte string makes the sums disagree with the download).
    fn publish(&self, tag: &str, payload: &[u8], sums_for: &[u8]) {
        let ver = tag.trim_start_matches('v');
        let name = format!("razerctl-{ver}-x86_64.AppImage");
        let json = release_json(&self.base, tag, ver, payload.len());
        self.put("/repos/Dromares/razerctl/releases/latest", 200, json);
        self.put(&format!("/dl/{tag}/{name}"), 200, payload);
        let sums = format!("{}  {name}\n{}  70-razerctl.rules\n", hex(sums_for), hex(b"rules"));
        self.put(&format!("/dl/{tag}/SHA256SUMS"), 200, sums);
    }
}

fn hex(b: &[u8]) -> String {
    sha256::hex_digest(b).unwrap()
}

fn release_json(base: &str, tag: &str, ver: &str, size: usize) -> String {
    format!(
        r#"{{
  "url": "https://api.github.com/repos/Dromares/razerctl/releases/415621196",
  "assets_url": "https://api.github.com/repos/Dromares/razerctl/releases/415621196/assets",
  "upload_url": "https://uploads.github.com/repos/Dromares/razerctl/releases/415621196/assets{{?name,label}}",
  "html_url": "https://github.com/Dromares/razerctl/releases/tag/{tag}",
  "id": 415621196,
  "author": {{ "login": "Dromares", "id": 55354285, "type": "User", "site_admin": false }},
  "node_id": "RE_kwDOVIX5zM4YxeBM",
  "tag_name": "{tag}",
  "target_commitish": "4e41dfe68591e7cad575790a892eb7d7c60d8fd5",
  "name": "razerctl {tag}",
  "draft": false,
  "immutable": false,
  "prerelease": false,
  "created_at": "2026-10-17T05:59:17Z",
  "updated_at": "2026-10-17T15:38:41Z",
  "published_at": "2026-10-17T15:23:45Z",
  "assets": [
    {{ "url": "https://api.github.com/repos/Dromares/razerctl/releases/assets/1", "id": 1, "name": "70-razerctl.rules",
       "label": "", "uploader": {{ "login": "Dromares" }}, "content_type": "application/octet-stream", "state": "uploaded",
       "size": 724, "digest": "sha256:f2fedb51", "download_count": 0, "browser_download_url": "{base}/dl/{tag}/70-razerctl.rules" }},
    {{ "id": 2, "name": "razerctl-{ver}-x86_64.AppImage", "content_type": "application/vnd.appimage", "state": "uploaded",
       "size": {size}, "browser_download_url": "{base}/dl/{tag}/razerctl-{ver}-x86_64.AppImage" }},
    {{ "id": 3, "name": "razerctl-{ver}-x86_64.AppImage.zsync", "size": 100,
       "browser_download_url": "{base}/dl/{tag}/razerctl-{ver}-x86_64.AppImage.zsync" }},
    {{ "id": 4, "name": "SHA256SUMS", "size": 200, "browser_download_url": "{base}/dl/{tag}/SHA256SUMS" }}
  ],
  "tarball_url": "https://api.github.com/repos/Dromares/razerctl/tarball/{tag}",
  "zipball_url": "https://api.github.com/repos/Dromares/razerctl/zipball/{tag}",
  "body": "First line.\r\n\r\n- fixed a thing\r\n- added another\r\n"
}}"#
    )
}

const LATEST: &str = "/repos/Dromares/razerctl/releases/latest";

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("razerctl-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn names(&self, rel: &str) -> Vec<String> {
        let mut n: Vec<String> = std::fs::read_dir(self.path(rel))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        n.sort();
        n
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        // a test that chmod'ed a directory read-only must still be able to clean up
        for rel in ["ro", "."] {
            let _ = std::fs::set_permissions(self.path(rel), std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ------------------------------------------------------------------------------------------------ the check

#[test]
fn finds_a_newer_release_and_reads_its_notes_and_assets() {
    let fx = Fixture::start();
    fx.publish("v0.3.0", b"new", b"new");
    let (res, body) = check(&fx.src(), &v("0.2.0"));
    let CheckResult::Available(r) = res else {
        panic!("expected an update, got {res:?}");
    };
    assert!(body.is_some());
    assert_eq!(r.tag, "v0.3.0");
    assert_eq!(r.version, v("0.3.0"));
    assert_eq!(r.notes_text(), "First line.\n\n- fixed a thing\n- added another");
    assert_eq!(r.published, "2026-10-17T15:23:45Z");
    assert_eq!(r.page, "https://github.com/Dromares/razerctl/releases/tag/v0.3.0");
    assert_eq!(r.appimage().unwrap().size, 3);
    assert_eq!(r.appimage().unwrap().url, format!("{}/dl/v0.3.0/razerctl-0.3.0-x86_64.AppImage", fx.base));
    assert!(r.sums().is_some());
    assert_eq!(fx.requests(), [LATEST], "one request, to /releases/latest");
}

#[test]
fn the_same_or_an_older_release_is_up_to_date_and_pre_releases_compare_by_semver() {
    let fx = Fixture::start();
    fx.publish("v0.2.0", b"x", b"x");
    assert!(matches!(check(&fx.src(), &v("0.2.0")).0, CheckResult::UpToDate(_)));
    assert!(matches!(check(&fx.src(), &v("0.3.0")).0, CheckResult::UpToDate(_)), "never a downgrade");
    // the Step 3 test tags: .1 < .2 < the release
    fx.publish("v0.2.91-test.2", b"x", b"x");
    assert!(matches!(check(&fx.src(), &v("0.2.90-test.1")).0, CheckResult::Available(_)));
    assert!(matches!(check(&fx.src(), &v("0.2.91-test.2")).0, CheckResult::UpToDate(_)));
    assert!(matches!(check(&fx.src(), &v("0.3.0")).0, CheckResult::UpToDate(_)), "0.3.0 is newer than 0.2.91-test.2");
    fx.publish("v0.3.0", b"x", b"x");
    assert!(matches!(check(&fx.src(), &v("0.2.91-test.2")).0, CheckResult::Available(_)));
    assert!(matches!(check(&fx.src(), &v("0.3.0-rc.1")).0, CheckResult::Available(_)));
}

#[test]
fn offline_a_rate_limit_and_a_404_are_failures_with_their_own_outcome() {
    let fx = Fixture::start();
    // 404 is what the anonymous probe of the private repo returns today
    let (res, body) = check(&fx.src(), &v("0.2.0"));
    assert_eq!(res, CheckResult::Failed(FetchError::NotFound));
    assert_eq!((res.outcome(), body), (Outcome::Failed, None));
    fx.put(LATEST, 403, r#"{"message":"API rate limit exceeded"}"#);
    assert_eq!(check(&fx.src(), &v("0.2.0")).0.outcome(), Outcome::RateLimited);
    fx.put(LATEST, 429, "");
    assert_eq!(check(&fx.src(), &v("0.2.0")).0.outcome(), Outcome::RateLimited);
    fx.put(LATEST, 500, "boom");
    assert_eq!(check(&fx.src(), &v("0.2.0")).0.outcome(), Outcome::Failed);
    fx.put(LATEST, 200, "<html>captive portal</html>");
    assert_eq!(check(&fx.src(), &v("0.2.0")).0.outcome(), Outcome::Failed);
    // nothing listening: bind a port and drop the listener
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", l.local_addr().unwrap().port())
    };
    assert_eq!(check(&Source::loopback(&dead), &v("0.2.0")).0.outcome(), Outcome::Offline);
}

#[test]
fn the_check_runs_once_in_24_hours() {
    let fx = Fixture::start();
    fx.publish("v0.3.0", b"x", b"x");
    let dir = Dir::new("throttle");
    let paths = Paths::in_dir(dir.path("cfg"));
    let t0 = 1_760_000_000;
    let cur = v("0.2.0");

    let first = check_if_due(&fx.src(), &paths, &cur, t0, true);
    assert!(matches!(first, Some(CheckResult::Available(_))));
    assert_eq!(fx.count(LATEST), 1);
    assert_eq!(paths.load().last_check, Some(t0));
    assert_eq!(paths.load().outcome, Some(Outcome::Available));

    assert!(check_if_due(&fx.src(), &paths, &cur, t0 + 60, true).is_none());
    assert!(check_if_due(&fx.src(), &paths, &cur, t0 + INTERVAL_SECS - 1, true).is_none());
    assert_eq!(fx.count(LATEST), 1, "no second request inside 24 h");

    assert!(check_if_due(&fx.src(), &paths, &cur, t0 + INTERVAL_SECS, true).is_some());
    assert_eq!(fx.count(LATEST), 2, "due again after 24 h");
    assert_eq!(paths.load().last_check, Some(t0 + INTERVAL_SECS));
}

#[test]
fn a_failed_check_also_uses_up_the_day_and_a_manual_check_ignores_the_throttle() {
    let fx = Fixture::start(); // no routes: every answer is a 404
    let dir = Dir::new("failed");
    let paths = Paths::in_dir(dir.path("cfg"));
    let cur = v("0.2.0");
    assert!(check_if_due(&fx.src(), &paths, &cur, 1000, true).is_some());
    assert_eq!(paths.load().outcome, Some(Outcome::Failed));
    assert!(check_if_due(&fx.src(), &paths, &cur, 2000, true).is_none(), "an offline start does not retry in a loop");
    assert_eq!(fx.count(LATEST), 1);
    // Check now: not throttled, and it records its own time
    fx.publish("v0.3.0", b"x", b"x");
    assert!(matches!(check_and_record(&fx.src(), &paths, &cur, 2000), CheckResult::Available(_)));
    assert_eq!(fx.count(LATEST), 2);
    assert_eq!(paths.load().last_check, Some(2000));
}

#[test]
fn the_off_switch_means_no_request_at_all() {
    let fx = Fixture::start();
    fx.publish("v0.3.0", b"x", b"x");
    let dir = Dir::new("off");
    let paths = Paths::in_dir(dir.path("cfg"));
    assert!(check_if_due(&fx.src(), &paths, &v("0.2.0"), 1_760_000_000, false).is_none());
    assert!(fx.requests().is_empty(), "disabled: nothing sent, got {:?}", fx.requests());
    assert!(!paths.state.exists(), "and nothing written");
}

#[test]
fn the_dialog_can_be_rebuilt_from_the_cache_and_not_now_lasts_a_day_unless_a_newer_release_comes() {
    let fx = Fixture::start();
    fx.publish("v0.3.0", b"x", b"x");
    let dir = Dir::new("cache");
    let paths = Paths::in_dir(dir.path("cfg"));
    let cur = v("0.2.0");
    assert_eq!(pending_release(&paths, &cur), None);
    check_and_record(&fx.src(), &paths, &cur, 1000);
    let r = pending_release(&paths, &cur).expect("cached");
    assert_eq!(r.tag, "v0.3.0");
    assert_eq!(pending_release(&paths, &v("0.3.0")), None, "updated by hand since: nothing pending");

    dismiss(&paths, 5000);
    assert!(!paths.load().banner_wanted(5000 + 60));
    assert!(paths.load().banner_wanted(5000 + INTERVAL_SECS));
    // the same release found again keeps the dismissal; a newer one clears it
    check_and_record(&fx.src(), &paths, &cur, 6000);
    assert_eq!(paths.load().dismissed, Some(5000));
    fx.publish("v0.3.1", b"x", b"x");
    check_and_record(&fx.src(), &paths, &cur, 7000);
    assert_eq!(paths.load().dismissed, None);
}

// ------------------------------------------------------------------------------------------------ the update

const OLD: &[u8] = b"OLD razerctl 0.2.0";
const NEW: &[u8] = b"NEW razerctl 0.3.0 \x7fELF payload";

/// A fixture serving 0.3.0 and an installed 0.2.0 AppImage at `dir/apps/razerctl-0.2.0-x86_64.AppImage`.
fn setup(sums_for: &[u8], dir: &Dir) -> (Fixture, razerctl::update::Release, PathBuf) {
    let fx = Fixture::start();
    fx.publish("v0.3.0", NEW, sums_for);
    let CheckResult::Available(rel) = check(&fx.src(), &v("0.2.0")).0 else {
        panic!("fixture release not found");
    };
    std::fs::create_dir_all(dir.path("apps")).unwrap();
    let target = dir.path("apps/razerctl-0.2.0-x86_64.AppImage");
    std::fs::write(&target, OLD).unwrap();
    (fx, rel, target)
}

fn run(fx: &Fixture, rel: &razerctl::update::Release, target: &Path, fallback: &Path) -> (Result<Applied, UpdateError>, Vec<Stage>) {
    let mut stages = Vec::new();
    let r = apply_appimage(&fx.src(), rel, target, fallback, &AtomicBool::new(false), &mut |s| {
        if !matches!((stages.last(), s), (Some(Stage::Downloading { .. }), Stage::Downloading { .. })) {
            stages.push(s);
        }
    });
    (r, stages)
}

#[test]
fn a_verified_download_replaces_the_file_in_place_and_keeps_the_old_one() {
    let dir = Dir::new("swap");
    let (fx, rel, target) = setup(NEW, &dir);
    let (res, stages) = run(&fx, &rel, &target, &dir.path("dl"));
    let old = dir.path("apps/razerctl-0.2.0-x86_64.AppImage.old");
    assert_eq!(res, Ok(Applied::Replaced { path: target.clone(), old: Some(old.clone()) }));
    assert_eq!(std::fs::read(&target).unwrap(), NEW);
    assert_eq!(std::fs::read(&old).unwrap(), OLD, "the previous file is kept as <name>.old");
    assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(dir.names("apps"), ["razerctl-0.2.0-x86_64.AppImage", "razerctl-0.2.0-x86_64.AppImage.old"], "no .part left");
    assert!(matches!(stages[0], Stage::Downloading { .. }) && stages[1..] == [Stage::Verifying, Stage::Replacing], "{stages:?}");
    assert!(!dir.path("dl").exists(), "the fallback folder is not touched");

    // a second update replaces the previous .old
    std::fs::write(&target, b"0.3.0 on disk").unwrap();
    assert!(run(&fx, &rel, &target, &dir.path("dl")).0.is_ok());
    assert_eq!(std::fs::read(&old).unwrap(), b"0.3.0 on disk");
}

#[test]
fn a_checksum_mismatch_refuses_and_changes_nothing() {
    let dir = Dir::new("mismatch");
    // SHA256SUMS lists a different file than the one the server sends
    let (fx, rel, target) = setup(b"some other bytes", &dir);
    let (res, stages) = run(&fx, &rel, &target, &dir.path("dl"));
    assert_eq!(
        res,
        Err(UpdateError::Mismatch { expected: hex(b"some other bytes"), got: hex(NEW) }),
        "{res:?}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), OLD, "the installed file is exactly as it was");
    assert_eq!(dir.names("apps"), ["razerctl-0.2.0-x86_64.AppImage"], "no .old, no .part, no new file");
    assert!(!dir.path("dl").exists(), "and nothing was saved to the fallback folder");
    assert_eq!(stages.last(), Some(&Stage::Verifying), "it never reached the replace step: {stages:?}");
}

#[test]
fn a_release_without_usable_checksums_is_refused_before_any_download() {
    let dir = Dir::new("nosums");
    let (fx, rel, target) = setup(NEW, &dir);
    // SHA256SUMS exists but has no line for the AppImage
    fx.put("/dl/v0.3.0/SHA256SUMS", 200, format!("{}  70-razerctl.rules\n", hex(b"rules")));
    let (res, _) = run(&fx, &rel, &target, &dir.path("dl"));
    assert!(matches!(res, Err(UpdateError::MissingAsset(_))), "{res:?}");
    assert_eq!(fx.count("/dl/v0.3.0/razerctl-0.3.0-x86_64.AppImage"), 0, "the big download never started");
    // SHA256SUMS missing from the server
    fx.put("/dl/v0.3.0/SHA256SUMS", 404, "");
    assert!(matches!(run(&fx, &rel, &target, &dir.path("dl")).0, Err(UpdateError::Net(FetchError::NotFound))));
    assert_eq!(std::fs::read(&target).unwrap(), OLD);
    // the release lists no SHA256SUMS asset at all
    let mut bare = rel.clone();
    bare.assets.retain(|a| a.name != "SHA256SUMS");
    assert!(matches!(run(&fx, &bare, &target, &dir.path("dl")).0, Err(UpdateError::MissingAsset(n)) if n == "SHA256SUMS"));
    assert_eq!(dir.names("apps"), ["razerctl-0.2.0-x86_64.AppImage"]);
}

#[test]
fn an_unwritable_directory_falls_back_to_downloaded_to() {
    let dir = Dir::new("readonly");
    let (fx, rel, target) = setup(NEW, &dir);
    let apps = dir.path("apps");
    std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::File::create(apps.join(".probe")).is_ok() {
        std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("running as a user that ignores directory modes (root): the read-only case is covered by the missing-directory test");
        return;
    }
    let fallback = dir.path("Downloads");
    let (res, stages) = run(&fx, &rel, &target, &fallback);
    let saved = fallback.join("razerctl-0.3.0-x86_64.AppImage");
    match &res {
        Ok(Applied::Saved { path, why }) => {
            assert_eq!(path, &saved);
            assert!(why.contains("not writable"), "{why}");
        }
        other => panic!("expected the fallback, got {other:?}"),
    }
    assert_eq!(std::fs::read(&saved).unwrap(), NEW, "saved, verified");
    assert_eq!(std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777, 0o755, "ready to run");
    assert_eq!(std::fs::read(&target).unwrap(), OLD, "the running file is untouched");
    assert_eq!(dir.names("Downloads"), ["razerctl-0.3.0-x86_64.AppImage"], "no .part left behind");
    assert!(!stages.contains(&Stage::Replacing), "{stages:?}");
}

#[test]
fn a_target_directory_that_is_gone_also_falls_back() {
    let dir = Dir::new("gone");
    let (fx, rel, _) = setup(NEW, &dir);
    let target = dir.path("not-there/razerctl-0.2.0-x86_64.AppImage");
    let (res, _) = run(&fx, &rel, &target, &dir.path("Downloads"));
    assert!(matches!(res, Ok(Applied::Saved { .. })), "{res:?}");
    assert_eq!(std::fs::read(dir.path("Downloads/razerctl-0.3.0-x86_64.AppImage")).unwrap(), NEW);
    assert!(!target.exists());
}

#[test]
fn a_cancelled_download_leaves_nothing() {
    let dir = Dir::new("cancel");
    let (fx, rel, target) = setup(NEW, &dir);
    let cancel = AtomicBool::new(true);
    let r = apply_appimage(&fx.src(), &rel, &target, &dir.path("dl"), &cancel, &mut |_| {});
    assert_eq!(r, Err(UpdateError::Cancelled));
    assert_eq!(dir.names("apps"), ["razerctl-0.2.0-x86_64.AppImage"]);
    assert_eq!(std::fs::read(&target).unwrap(), OLD);
}

#[test]
fn asset_urls_must_be_https_unless_the_source_is_the_loopback_fixture() {
    let dir = Dir::new("https");
    let (_fx, rel, target) = setup(NEW, &dir);
    // the real source refuses the fixture's http:// asset URLs outright
    let r = apply_appimage(&Source::github(), &rel, &target, &dir.path("dl"), &AtomicBool::new(false), &mut |_| {});
    assert!(matches!(&r, Err(UpdateError::Net(FetchError::Other(m))) if m.contains("non-https")), "{r:?}");
    assert_eq!(std::fs::read(&target).unwrap(), OLD);
}

// ------------------------------------------------------------------------------------------------ the CLI

fn cli(fx: &Fixture, args: &[&str], install: &InstallType, answer: &str) -> (Result<(), String>, String) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let mut out = Vec::new();
    let r = razerctl::update::cli::run(&args, &fx.src(), &v("0.2.0"), install, &mut answer.as_bytes(), &mut out);
    (r, String::from_utf8(out).unwrap())
}

fn appimage_install(dir: &Dir) -> (InstallType, PathBuf) {
    std::fs::create_dir_all(dir.path("apps")).unwrap();
    let target = dir.path("apps/razerctl-0.2.0-x86_64.AppImage");
    std::fs::write(&target, OLD).unwrap();
    (InstallType::AppImage { path: target.clone() }, target)
}

#[test]
fn update_check_makes_one_request_and_changes_nothing() {
    let dir = Dir::new("cli-check");
    let fx = Fixture::start();
    fx.publish("v0.3.0", NEW, NEW);
    let (install, target) = appimage_install(&dir);
    let (res, out) = cli(&fx, &["--check"], &install, "y\n");
    assert_eq!(res, Ok(()));
    assert!(out.contains("razerctl 0.3.0 is available"), "{out}");
    assert!(!out.contains("[y/N]"), "--check never asks");
    assert_eq!(fx.requests(), [LATEST]);
    assert_eq!(std::fs::read(&target).unwrap(), OLD);
}

#[test]
fn update_asks_before_doing_anything_and_n_changes_nothing() {
    let dir = Dir::new("cli-no");
    let fx = Fixture::start();
    fx.publish("v0.3.0", NEW, NEW);
    let (install, target) = appimage_install(&dir);
    for answer in ["n\n", "\n", "", "later\n"] {
        let (res, out) = cli(&fx, &[], &install, answer);
        assert_eq!(res, Ok(()));
        assert!(out.contains("[y/N]") && out.contains("Nothing was changed."), "{out}");
        assert!(out.contains("- fixed a thing"), "the notes come before the question");
        assert_eq!(std::fs::read(&target).unwrap(), OLD, "answer {answer:?}");
    }
    assert!(
        fx.requests().iter().all(|h| h == LATEST),
        "nothing was downloaded without a yes: {:?}",
        fx.requests()
    );
}

#[test]
fn update_with_a_yes_downloads_verifies_and_replaces() {
    let dir = Dir::new("cli-yes");
    let fx = Fixture::start();
    fx.publish("v0.3.0", NEW, NEW);
    let (install, target) = appimage_install(&dir);
    let (res, out) = cli(&fx, &[], &install, "y\n");
    assert_eq!(res, Ok(()));
    assert!(out.contains("is installed at") && out.contains("Restart razerctl"), "{out}");
    assert_eq!(std::fs::read(&target).unwrap(), NEW);
    assert_eq!(std::fs::read(dir.path("apps/razerctl-0.2.0-x86_64.AppImage.old")).unwrap(), OLD);
}

#[test]
fn update_with_a_yes_and_a_bad_checksum_fails_and_keeps_the_file() {
    let dir = Dir::new("cli-bad");
    let fx = Fixture::start();
    fx.publish("v0.3.0", NEW, b"some other file");
    let (install, target) = appimage_install(&dir);
    let (res, _) = cli(&fx, &[], &install, "y\n");
    assert!(res.unwrap_err().contains("does not match SHA256SUMS"));
    assert_eq!(std::fs::read(&target).unwrap(), OLD);
    assert_eq!(dir.names("apps"), ["razerctl-0.2.0-x86_64.AppImage"]);
}

#[test]
fn package_and_tarball_installs_are_told_where_and_never_asked() {
    let fx = Fixture::start();
    fx.publish("v0.3.0", NEW, NEW);
    let (res, out) = cli(&fx, &[], &InstallType::Package, "y\n");
    assert_eq!(res, Ok(()));
    assert!(out.contains("package manager") && !out.contains("[y/N]"), "{out}");
    let (res, out) = cli(&fx, &[], &InstallType::Tarball, "y\n");
    assert_eq!(res, Ok(()));
    assert!(out.contains("releases/tag/v0.3.0") && !out.contains("[y/N]"), "{out}");
    assert!(fx.requests().iter().all(|h| h == LATEST), "neither downloads: {:?}", fx.requests());
}

#[test]
fn update_says_when_it_is_current_and_why_a_check_failed() {
    let fx = Fixture::start();
    fx.publish("v0.2.0", NEW, NEW);
    let (res, out) = cli(&fx, &["--check"], &InstallType::Tarball, "");
    assert_eq!(res, Ok(()));
    assert!(out.contains("up to date"), "{out}");
    let down = Fixture::start();
    let (res, _) = cli(&down, &["--check"], &InstallType::Tarball, "");
    assert!(res.unwrap_err().contains("couldn't check for updates"), "an explicit check does say why");
    assert!(cli(&fx, &["--bogus"], &InstallType::Tarball, "").0.is_err());
}
