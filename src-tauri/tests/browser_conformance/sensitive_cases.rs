//! Sensitive-action cases: the host classifies each from the page, asks
//! for confirmation (answered as unattended here), honours pre-approvals
//! at the scope of the tab's profile, refuses secret or out-of-project
//! uploads, and lands downloads under app data with host-chosen names and
//! the OS's mark.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;
use termul_manager_lib::conformance_support::{ActionClass, AppPreapproval, CanonicalOrigin};

use crate::adapter::{Scope, LINKED_FILE, SECRET_FILE, SWAPPED_FILE, UPLOAD_BODY, UPLOAD_FILE};
use crate::cases::{mismatch, CaseResult, Ctx, Failure};

const PASSWORD_READY: &str = "Password fixture";
const UPLOAD_READY: &str = "Upload fixture";
const DOWNLOAD_READY: &str = "Download fixture";
const DOWNLOAD_WAIT: Duration = Duration::from_secs(15);

fn origin(c: &Ctx<'_>) -> String {
    c.fixtures.url("").trim_end_matches('/').to_string()
}

/// The call failed with `code` and `reason`. A failure with another code
/// is the case's result as it is, so an engine without the verb reports
/// `capability_unavailable`.
fn expect_refusal(
    result: Result<serde_json::Value, Failure>,
    code: &str,
    reason: &str,
    what: &str,
) -> CaseResult {
    match result {
        Err(f) if f.code == code && f.reason.as_deref() == Some(reason) => Ok(()),
        Err(f) if f.code != code => Err(f),
        Err(f) => Err(mismatch(format!(
            "{what}: expected {code}/{reason}, got {}/{:?}",
            f.code, f.reason
        ))),
        Ok(_) => Err(mismatch(format!(
            "{what}: expected {code}/{reason}, call succeeded"
        ))),
    }
}

fn expect_tickets(c: &Ctx<'_>, expected: &[ActionClass], what: &str) -> CaseResult {
    let raised: Vec<ActionClass> = c
        .engine
        .tickets(&c.session)
        .iter()
        .map(|t| t.action)
        .collect();
    if raised == expected {
        Ok(())
    } else {
        Err(mismatch(format!(
            "{what}: tickets {raised:?}, expected {expected:?}"
        )))
    }
}

/// Open `path` on a tab of `profile` ("global" or "ephemeral").
async fn open_on(c: &Ctx<'_>, profile: &str, path: &str, ready: &str) -> CaseResult {
    if profile == "ephemeral" {
        c.engine.go_ephemeral(&c.session).await.map_err(mismatch)?;
    }
    let reply = c.navigate(path).await?;
    if reply["tab"]["profile"] != profile {
        return Err(mismatch(format!(
            "the tab runs on the {} profile, not {profile}",
            reply["tab"]["profile"]
        )));
    }
    c.call("wait", json!({ "text": ready })).await.map(|_| ())
}

async fn preapprove(c: &Ctx<'_>, scope: Scope, action: ActionClass) -> CaseResult {
    c.engine
        .preapprove(&c.session, scope, action, &origin(c))
        .await
        .map_err(mismatch)
}

async fn click_sign_in(c: &Ctx<'_>) -> Result<serde_json::Value, Failure> {
    let submit = c.ref_of("button", "Sign in").await?;
    c.call("click", json!({ "ref": submit })).await
}

pub async fn password_submit(c: &Ctx<'_>) -> CaseResult {
    open_on(c, "ephemeral", "/password.html", PASSWORD_READY).await?;
    expect_refusal(
        click_sign_in(c).await,
        "confirmation_required",
        "unattended",
        "password submit",
    )?;
    expect_tickets(c, &[ActionClass::PasswordSubmit], "password submit")?;
    c.expect_url("/password.html", "the refused submit").await?;

    preapprove(c, Scope::Project, ActionClass::PasswordSubmit).await?;
    click_sign_in(c).await?;
    c.call("wait", json!({ "text": "Form done" })).await?;
    expect_tickets(
        c,
        &[ActionClass::PasswordSubmit],
        "pre-approved password submit",
    )
}

pub async fn password_submit_global(c: &Ctx<'_>) -> CaseResult {
    open_on(c, "global", "/password.html", PASSWORD_READY).await?;
    preapprove(c, Scope::Project, ActionClass::PasswordSubmit).await?;
    expect_refusal(
        click_sign_in(c).await,
        "confirmation_required",
        "unattended",
        "pre-approved password submit on the global profile",
    )?;
    expect_tickets(c, &[ActionClass::PasswordSubmit], "global password submit")?;
    let parsed = CanonicalOrigin::parse(&origin(c)).map_err(|e| mismatch(format!("{e:?}")))?;
    if AppPreapproval::new(ActionClass::PasswordSubmit, parsed).is_ok() {
        return Err(mismatch(
            "a password submit could be pre-approved for every project",
        ));
    }
    Ok(())
}

pub async fn tainted_navigation(c: &Ctx<'_>) -> CaseResult {
    c.open("/form.html", "Conformance form").await?;
    c.snapshot().await?;
    let away = format!(
        "http://localhost:{}/index.html?d=page-data",
        c.fixtures.port()
    );
    let reply = c.call("navigate", json!({ "url": away })).await;
    expect_refusal(
        reply,
        "confirmation_required",
        "unattended",
        "tainted cross-origin navigation",
    )?;
    expect_tickets(c, &[ActionClass::TaintedExfil], "tainted navigation")?;
    c.call(
        "navigate",
        json!({ "url": c.fixtures.url("/index.html?q=1") }),
    )
    .await
    .map(|_| ())
}

pub async fn upload(c: &Ctx<'_>) -> CaseResult {
    open_on(c, "ephemeral", "/upload.html", UPLOAD_READY).await?;
    let input = c.ref_of("textbox", "Attachment").await?;
    let reply = c
        .call("upload", json!({ "ref": input, "path": UPLOAD_FILE }))
        .await;
    expect_refusal(reply, "confirmation_required", "unattended", "upload")?;
    expect_tickets(c, &[ActionClass::Upload], "upload")?;

    preapprove(c, Scope::Project, ActionClass::Upload).await?;
    let input = c.ref_of("textbox", "Attachment").await?;
    let reply = c
        .call("upload", json!({ "ref": input, "path": UPLOAD_FILE }))
        .await?;
    if reply["fidelity"] != "trusted" {
        return Err(mismatch(format!(
            "upload fidelity is {}",
            reply["fidelity"]
        )));
    }
    expect_tickets(c, &[ActionClass::Upload], "pre-approved upload")?;
    c.expect_text(&format!(
        "Picked: {UPLOAD_FILE} ({} bytes)",
        UPLOAD_BODY.len()
    ))
    .await
}

pub async fn upload_refusals(c: &Ctx<'_>) -> CaseResult {
    c.open("/upload.html", UPLOAD_READY).await?;
    let input = c.ref_of("textbox", "Attachment").await?;
    let secret = c
        .call("upload", json!({ "ref": input, "path": SECRET_FILE }))
        .await;
    expect_refusal(secret, "invalid_params", "secret_file", "secret upload")?;
    if !c.engine.has_symlink() {
        println!(
            "upload_refusals: symlinks cannot be created here; the symlink checks are unit-tested"
        );
        return expect_tickets(c, &[], "refused uploads");
    }
    let linked = c
        .call("upload", json!({ "ref": input, "path": LINKED_FILE }))
        .await;
    expect_refusal(
        linked,
        "invalid_params",
        "outside_project_root",
        "symlinked upload",
    )?;
    expect_tickets(c, &[], "refused uploads")?;

    c.engine.swap_upload_while_asking(&c.session);
    let swapped = c
        .call("upload", json!({ "ref": input, "path": SWAPPED_FILE }))
        .await;
    expect_refusal(
        swapped,
        "invalid_params",
        "outside_project_root",
        "upload swapped for a symlink while awaiting consent",
    )?;
    expect_tickets(c, &[ActionClass::Upload], "swapped upload")?;
    c.expect_text("Picked: none").await
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

async fn until(what: &str, mut done: impl FnMut() -> bool) -> CaseResult {
    let deadline = Instant::now() + DOWNLOAD_WAIT;
    while !done() {
        if Instant::now() > deadline {
            return Err(mismatch(format!(
                "{what} did not happen within {}s",
                DOWNLOAD_WAIT.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

async fn start_download(c: &Ctx<'_>) -> CaseResult {
    c.open("/download.html", DOWNLOAD_READY).await?;
    let link = c.ref_of("link", "Get the file").await?;
    c.call("click", json!({ "ref": link })).await.map(|_| ())
}

pub async fn download(c: &Ctx<'_>) -> CaseResult {
    start_download(c).await?;
    until("a download consent ticket", || {
        c.engine
            .tickets(&c.session)
            .iter()
            .any(|t| t.action == ActionClass::Download)
    })
    .await?;
    let dir = c.engine.downloads_dir(&c.session);
    until("deleting the unconfirmed download", || {
        files_in(&dir).is_empty()
    })
    .await
}

#[cfg(windows)]
fn has_mark(file: &Path) -> bool {
    let mut stream = file.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    std::fs::read_to_string(PathBuf::from(stream)).is_ok_and(|zone| zone.contains("ZoneId=3"))
}

#[cfg(target_os = "macos")]
fn has_mark(file: &Path) -> bool {
    std::process::Command::new("xattr")
        .args(["-p", "com.apple.quarantine"])
        .arg(file)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Linux has no OS quarantine to check.
#[cfg(target_os = "linux")]
fn has_mark(_file: &Path) -> bool {
    true
}

pub async fn download_preapproved(c: &Ctx<'_>) -> CaseResult {
    preapprove(c, Scope::App, ActionClass::Download).await?;
    start_download(c).await?;
    let dir = c.engine.downloads_dir(&c.session);
    until("the download landing under app data", || {
        files_in(&dir).iter().any(|f| has_mark(f))
    })
    .await?;
    let files = files_in(&dir);
    let [file] = files.as_slice() else {
        return Err(mismatch(format!("downloads dir holds {files:?}")));
    };
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    let host_named = name
        .strip_suffix(".bin")
        .is_some_and(|stem| uuid::Uuid::parse_str(stem).is_ok());
    if !host_named {
        return Err(mismatch(format!("download named {name}, not <uuid>.bin")));
    }
    let body = std::fs::read_to_string(file).unwrap_or_default();
    if body != crate::fixture_server::DOWNLOAD_BODY {
        return Err(mismatch("downloaded bytes differ from the served file"));
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    if !file.exists() {
        return Err(mismatch("the pre-approved download was deleted"));
    }
    expect_tickets(c, &[], "pre-approved download")
}
