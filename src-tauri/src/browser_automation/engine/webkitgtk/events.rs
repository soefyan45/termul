//! WebKitGTK tab events tauri does not expose: same-document URL changes,
//! and the downloads of this tab.
//!
//! `notify::uri` fires for every URL change; while a load is in progress
//! the change belongs to that load (its commit carries the URL), so only
//! changes outside a load are same-document commits.
//!
//! Downloads start on the web context every tab shares, so each tab
//! listens there and keeps only the downloads its own webview started.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use gtk::glib::prelude::*;
use gtk::prelude::WidgetExt;
use tauri::webview::PlatformWebview;
use tauri::Url;
use webkit2gtk::{Download, DownloadExt, URIRequestExt, WebContextExt, WebViewExt};

use crate::browser_automation::engine::events::TabEvents;

pub fn attach(platform: &PlatformWebview, events: TabEvents) -> bool {
    let webview = platform.inner();
    if events.holds_downloads() {
        attach_downloads(&webview, events.clone());
    }
    let _ = webview.connect_notify_local(Some("uri"), move |webview, _| {
        if webview.property::<bool>("is-loading") {
            return;
        }
        if let Some(uri) = webview.property::<Option<String>>("uri") {
            events.same_document(&uri);
        }
    });
    true
}

fn download_url(download: &Download) -> Option<Url> {
    download.request()?.uri()?.parse().ok()
}

fn attach_downloads(webview: &webkit2gtk::WebView, events: TabEvents) {
    let Some(context) = webview.context() else {
        return;
    };
    let own = webview.downgrade();
    let handler = context.connect_download_started(move |_, download| {
        let Some(own) = own.upgrade() else {
            return;
        };
        if download.web_view().as_ref() != Some(&own) {
            return;
        }
        // The destination is set as the download starts: WebKitGTK does not
        // ask agent-tab downloads for one before it creates the file. The
        // 4.1 API takes it as a file URI, not a path.
        let destination = download_url(download)
            .and_then(|url| events.download_starting(&url, Path::new("")))
            .and_then(|path| Url::from_file_path(path).ok());
        let Some(destination) = destination else {
            download.cancel();
            return;
        };
        download.set_destination(destination.as_str());
        let failed = Rc::new(Cell::new(false));
        let failed_flag = failed.clone();
        download.connect_failed(move |_, _| {
            log::info!("[browser-agent] webkit download failed");
            failed_flag.set(true);
        });
        let finished = events.clone();
        download.connect_finished(move |download| {
            if let Some(url) = download_url(download) {
                finished.download_finished(&url, !failed.get());
            }
        });
    });
    let handler = RefCell::new(Some(handler));
    webview.connect_destroy(move |_| {
        if let Some(handler) = handler.take() {
            context.disconnect(handler);
        }
    });
}
