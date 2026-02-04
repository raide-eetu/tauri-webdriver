// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crate::cli::Args;
use anyhow::Error;
use futures_util::TryFutureExt;
use http_body_util::{BodyExt, Either, Full};
use hyper::{
  body::{Bytes, Incoming},
  header::CONTENT_LENGTH,
  http::uri::Authority,
  service::service_fn,
  Method, Request, Response,
};
use hyper_util::{
  client::legacy::{connect::HttpConnector, Client},
  rt::{TokioExecutor, TokioIo},
  server::conn::auto,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::process::Child;
use tokio::net::TcpListener;

const TAURI_OPTIONS: &str = "tauri:options";

type ResponseBody = Either<Full<Bytes>, Incoming>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TauriOptions {
  application: PathBuf,
  #[serde(default)]
  args: Vec<String>,
  #[cfg(target_os = "windows")]
  #[serde(default)]
  webview_options: Option<Value>,
}

impl TauriOptions {
  #[cfg(target_os = "linux")]
  fn into_native_object(self) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(
      "webkitgtk:browserOptions".into(),
      json!({"binary": self.application, "args": self.args}),
    );
    map
  }

  #[cfg(target_os = "windows")]
  fn into_native_object(self) -> Map<String, Value> {
    let mut ms_edge_options = Map::new();
    ms_edge_options.insert(
      "binary".into(),
      json!(self.application.with_extension("exe")),
    );
    ms_edge_options.insert("args".into(), self.args.into());

    if let Some(webview_options) = self.webview_options {
      ms_edge_options.insert("webviewOptions".into(), webview_options);
    }

    let mut map = Map::new();
    map.insert("ms:edgeChromium".into(), json!(true));
    map.insert("browserName".into(), json!("webview2"));
    map.insert("ms:edgeOptions".into(), ms_edge_options.into());
    map
  }

  #[cfg(target_os = "macos")]
  fn into_native_object(self) -> Map<String, Value> {
    // WDA expects the .app bundle path, not the binary inside it.
    // Strip paths like Foo.app/Contents/MacOS/Foo back to Foo.app.
    let app_path = {
      let s = self.application.to_string_lossy();
      if let Some(idx) = s.find(".app/") {
        PathBuf::from(&s[..idx + 4])
      } else {
        self.application
      }
    };

    let mut map = Map::new();

    // WDA needs bundleId to launch the app via XCUIApplication.
    // Read it from the .app bundle's Info.plist.
    if let Some(bundle_id) = read_bundle_id(&app_path) {
      map.insert("bundleId".into(), json!(bundle_id));
    }

    map.insert("appPath".into(), json!(app_path));
    map.insert("arguments".into(), json!(self.args));
    map.insert(
      "environment".into(),
      json!({
        "TAURI_WEBVIEW_AUTOMATION": "true",
        "TAURI_AUTOMATION": "true"
      }),
    );
    map
  }
}

async fn handle(
  client: Client<HttpConnector, Full<Bytes>>,
  req: Request<Incoming>,
  args: Args,
) -> Result<Response<ResponseBody>, Error> {
  // macOS: intercept W3C WebDriver endpoints that WDA doesn't support
  #[cfg(target_os = "macos")]
  if let Some(response) = macos_translate(req.method(), req.uri().path()) {
    // consume the incoming body so hyper doesn't complain
    let (_parts, body) = req.into_parts();
    let _ = body.collect().await;
    return Ok(response);
  }

  // manipulate a new session to convert options to the native driver format
  let new_req: Request<Full<Bytes>> =
    if let (&Method::POST, "/session") = (req.method(), req.uri().path()) {
      let (mut parts, body) = req.into_parts();

      // get the body from the future stream and parse it as json
      let body = body.collect().await?.to_bytes().to_vec();
      let json: Value = serde_json::from_slice(&body)?;

      // manipulate the json to convert from tauri option to native driver options
      let json = map_capabilities(json);

      // serialize json and update the content-length header to be accurate
      let bytes = serde_json::to_vec(&json)?;
      parts.headers.insert(CONTENT_LENGTH, bytes.len().into());

      Request::from_parts(parts, Full::new(bytes.into()))
    } else {
      #[allow(unused_mut)]
      let (mut parts, body) = req.into_parts();

      let body = body.collect().await?.to_bytes().to_vec();

      // macOS: translate Appium locator strategies to WDA-native names
      #[cfg(target_os = "macos")]
      let body = if parts.method == Method::POST && is_element_find(parts.uri.path()) {
        if let Ok(mut json) = serde_json::from_slice::<Value>(&body) {
          macos_rewrite_locator(&mut json);
          let bytes = serde_json::to_vec(&json).unwrap_or(body);
          parts.headers.insert(CONTENT_LENGTH, bytes.len().into());
          bytes
        } else {
          body
        }
      } else {
        body
      };

      Request::from_parts(parts, Full::new(body.into()))
    };

  client
    .request(forward_to_native_driver(new_req, args)?)
    .map_ok(|resp| resp.map(Either::Right))
    .err_into()
    .await
}

/// Transform the request to a request for the native webdriver server.
fn forward_to_native_driver(
  mut req: Request<Full<Bytes>>,
  args: Args,
) -> Result<Request<Full<Bytes>>, Error> {
  let host: Authority = {
    let headers = req.headers_mut();
    headers.remove("host").expect("hyper request has host")
  }
  .to_str()?
  .parse()?;

  let path = req
    .uri()
    .path_and_query()
    .expect("hyper request has uri")
    .clone();

  let uri = format!(
    "http://{}:{}{}",
    host.host(),
    args.native_port,
    path.as_str()
  );

  let (mut parts, body) = req.into_parts();
  parts.uri = uri.parse()?;
  Ok(Request::from_parts(parts, body))
}

/// macOS: translate W3C WebDriver endpoints that WDA doesn't support.
/// Returns Some(response) if the request was handled, None to proxy to WDA.
#[cfg(target_os = "macos")]
fn macos_translate(method: &Method, path: &str) -> Option<Response<ResponseBody>> {
  // strip /session/{id} prefix to match the endpoint
  let endpoint = path
    .strip_prefix("/session/")
    .and_then(|rest| rest.split_once('/'))
    .map(|(_, endpoint)| endpoint);

  match (method, endpoint) {
    // GET /session/{id}/window → getWindowHandle
    (&Method::GET, Some("window")) => Some(json_response(json!({"value": "main"}))),

    // GET /session/{id}/window/handles → getWindowHandles
    (&Method::GET, Some("window/handles")) => {
      Some(json_response(json!({"value": ["main"]})))
    }

    // GET /session/{id}/title → getTitle
    (&Method::GET, Some("title")) => Some(json_response(json!({"value": ""}))),

    // GET /session/{id}/url → getCurrentUrl
    (&Method::GET, Some("url")) => Some(json_response(json!({"value": ""}))),

    // DELETE /session/{id}/window → closeWindow
    (&Method::DELETE, Some("window")) => Some(json_response(json!({"value": []}))),

    // POST /session/{id}/window → switchToWindow (noop, single window)
    (&Method::POST, Some("window")) => Some(json_response(json!({"value": null}))),

    _ => None,
  }
}

/// Build a W3C WebDriver JSON response.
#[cfg(target_os = "macos")]
fn json_response(body: Value) -> Response<ResponseBody> {
  let bytes = serde_json::to_vec(&body).unwrap();
  Response::builder()
    .status(200)
    .header("Content-Type", "application/json; charset=utf-8")
    .header(CONTENT_LENGTH, bytes.len())
    .body(Either::Left(Full::new(bytes.into())))
    .unwrap()
}

/// Read CFBundleIdentifier from an .app bundle's Info.plist.
#[cfg(target_os = "macos")]
fn read_bundle_id(app_path: &std::path::Path) -> Option<String> {
  let plist = app_path.join("Contents/Info.plist");
  let output = std::process::Command::new("defaults")
    .arg("read")
    .arg(&plist)
    .arg("CFBundleIdentifier")
    .output()
    .ok()?;
  if output.status.success() {
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
  } else {
    None
  }
}

/// Check if the path is an element find endpoint.
#[cfg(target_os = "macos")]
fn is_element_find(path: &str) -> bool {
  let endpoint = match path
    .strip_prefix("/session/")
    .and_then(|rest| rest.split_once('/'))
    .map(|(_, ep)| ep)
  {
    Some(ep) => ep,
    None => return false,
  };

  endpoint == "element"
    || endpoint == "elements"
    || endpoint.ends_with("/element")
    || endpoint.ends_with("/elements")
}

/// Rewrite Appium-style locator strategies to WDA-native names.
#[cfg(target_os = "macos")]
fn macos_rewrite_locator(json: &mut Value) {
  if let Some(using) = json.get("using").and_then(|v| v.as_str()).map(String::from) {
    let translated = match using.as_str() {
      "-ios predicate string" => "predicate string",
      "-ios class chain" => "class chain",
      _ => return,
    };
    json["using"] = json!(translated);
  }
}

/// only happy path for now, no errors
fn map_capabilities(mut json: Value) -> Value {
  let mut native = None;
  if let Some(capabilities) = json.get_mut("capabilities") {
    if let Some(always_match) = capabilities.get_mut("alwaysMatch") {
      if let Some(always_match) = always_match.as_object_mut() {
        if let Some(tauri_options) = always_match.remove(TAURI_OPTIONS) {
          if let Ok(options) = serde_json::from_value::<TauriOptions>(tauri_options) {
            native = Some(options.into_native_object());
          }
        }

        if let Some(native) = native.clone() {
          always_match.extend(native);
        }
      }
    }
  }

  #[cfg(any(target_os = "linux", windows))]
  if let Some(native) = native {
    if let Some(desired) = json.get_mut("desiredCapabilities") {
      if let Some(desired) = desired.as_object_mut() {
        desired.remove(TAURI_OPTIONS);
        desired.extend(native);
      }
    }
  }

  json
}

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: Args, mut _driver: Child) -> Result<(), Error> {
  #[cfg(unix)]
  let (signals_handle, signals_task) = {
    use futures_util::StreamExt;
    use signal_hook::consts::signal::*;

    let signals = signal_hook_tokio::Signals::new([SIGTERM, SIGINT, SIGQUIT])?;
    let signals_handle = signals.handle();
    let signals_task = tokio::spawn(async move {
      let mut signals = signals.fuse();
      #[allow(clippy::never_loop)]
      while let Some(signal) = signals.next().await {
        match signal {
          SIGTERM | SIGINT | SIGQUIT => {
            _driver
              .kill()
              .expect("unable to kill native webdriver server");
            std::process::exit(0);
          }
          _ => unreachable!(),
        }
      }
    });
    (signals_handle, signals_task)
  };

  let address = std::net::SocketAddr::from(([127, 0, 0, 1], args.port));

  // the client we use to proxy requests to the native webdriver
  let client = Client::builder(TokioExecutor::new())
    .http1_preserve_header_case(true)
    .http1_title_case_headers(true)
    .retry_canceled_requests(false)
    .build_http();

  // set up a http1 server that uses the service we just created
  let srv = async move {
    if let Ok(listener) = TcpListener::bind(address).await {
      loop {
        let client = client.clone();
        let args = args.clone();
        if let Ok((stream, _)) = listener.accept().await {
          let io = TokioIo::new(stream);

          tokio::task::spawn(async move {
            if let Err(err) = auto::Builder::new(TokioExecutor::new())
              .http1()
              .title_case_headers(true)
              .preserve_header_case(true)
              .serve_connection(
                io,
                service_fn(|request| handle(client.clone(), request, args.clone())),
              )
              .await
            {
              println!("Error serving connection: {err:?}");
            }
          });
        } else {
          println!("accept new stream fail, ignore here");
        }
      }
    } else {
      println!("can not listen to address: {address:?}");
    }
  };
  srv.await;

  #[cfg(unix)]
  {
    signals_handle.close();
    signals_task.await?;
  }

  Ok(())
}
