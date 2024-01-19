use jsonrpc_ws_server::jsonrpc_core::{serde::Serialize, *};
use jsonrpc_ws_server::*;
use serde::{Deserialize, Deserializer};
use serde_json::json;
use serde_json::Value as JsonValue;
use tauri::http;
use tauri::ipc::CallbackFn;
use tauri::ipc::InvokeBody;
use tauri::ipc::InvokeResponder;
use tauri::ipc::InvokeResponse;
use tauri::window::InvokeRequest;
use tauri::{AppHandle, Manager, Runtime, Window};

#[derive(Serialize, Deserialize, Debug)]
struct InvokeRpcParams {
  id: usize,
  window_label: String,
  payload: String,
}

struct HeaderMap(http::HeaderMap);

impl<'de> Deserialize<'de> for HeaderMap {
  fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let map = std::collections::HashMap::<String, String>::deserialize(deserializer)?;
    let mut headers = http::HeaderMap::default();
    for (key, value) in map {
      if let (Ok(key), Ok(value)) = (
        http::HeaderName::from_bytes(key.as_bytes()),
        http::HeaderValue::from_str(&value),
      ) {
        headers.insert(key, value);
      } else {
        return Err(serde::de::Error::custom(format!(
          "invalid header `{key}` `{value}`"
        )));
      }
    }
    Ok(Self(headers))
  }
}
#[derive(Deserialize)]
struct RequestOptions {
  headers: HeaderMap,
}
#[derive(Deserialize)]
struct Message {
  cmd: String,
  callback: CallbackFn,
  error: CallbackFn,
  payload: serde_json::Map<String, JsonValue>,
  options: Option<RequestOptions>,
}

#[derive(Serialize, Deserialize)]
enum RpcResponseStatus {
  Processing,
  Success,
  Error,
  Invalid,
}

#[derive(Serialize, Deserialize)]
struct RpcResult {
  status: RpcResponseStatus,
  data: Value,
}

pub struct AwesomeRpc {
  port: u16,
  allowed_origins: DomainsValidation<Origin>,
}

impl AwesomeRpc {
  pub fn new(allowed_origins: Vec<&str>) -> Self {
    let port = portpicker::pick_unused_port().expect("failed to get unused port for invoke");
    let allowed_origins =
      DomainsValidation::AllowOnly(allowed_origins.iter().map(|i| i.into()).collect());

    Self {
      port,
      allowed_origins,
    }
  }

  pub fn start<R: Runtime>(&self, app_handle: AppHandle<R>) {
    let handle = app_handle.clone();

    let mut io = IoHandler::new();
    io.add_sync_method("invoke", move |params: Params| {
      let params = params.parse::<InvokeRpcParams>().unwrap();

      if let Some(window) = handle.get_window(params.window_label.as_str()) {
        let message = serde_json::from_str::<Message>(params.payload.as_str()).unwrap();
        window.on_message(
          InvokeRequest {
            cmd: message.cmd.clone(),
            callback: message.callback,
            error: message.error,
            body: InvokeBody::Json(message.payload.into()),
            headers: message.options.map(|o| o.headers.0).unwrap_or_default(),
          },
          Box::new(move |window, _cmd, response, _callback, _error| {
            #[derive(Serialize, Deserialize)]
            struct JsonRpcResponse {
              jsonrpc: String,
              id: usize,
              result: RpcResult,
            }

            let result = match response {
              InvokeResponse::Ok(InvokeBody::Json(r)) => RpcResult {
                status: RpcResponseStatus::Success,
                data: r.clone(),
              },
              InvokeResponse::Ok(InvokeBody::Raw(r)) => RpcResult {
                status: RpcResponseStatus::Success,
                data: JsonValue::String(std::str::from_utf8(&r.to_owned()).unwrap().to_string()),
              },
              InvokeResponse::Err(e) => RpcResult {
                status: RpcResponseStatus::Error,
                data: e.0.clone(),
              },
            };

            let r = JsonRpcResponse {
              jsonrpc: "2.0".into(),
              id: params.id,
              result,
            };

            window.state::<AwesomeEmit>().send(r);
          }),
        );

        return Ok(json!(RpcResult {
          status: RpcResponseStatus::Processing,
          data: Value::Null
        }));
      }

      Ok(json!(RpcResult {
        status: RpcResponseStatus::Invalid,
        data: Value::String("Malformed request".into())
      }))
    });

    let server = ServerBuilder::new(io)
      .allowed_origins(self.allowed_origins.clone())
      .start(&format!("0.0.0.0:{}", self.port).as_str().parse().unwrap())
      .expect("RPC server must start with no issues");

    app_handle.manage(AwesomeEmit::new(server.broadcaster()));

    tauri::async_runtime::spawn(async { server.wait().unwrap() });
  }

  pub fn responder<R: Runtime>() -> Box<InvokeResponder<R>> {
    let responder = |_window: &Window<R>,
                     _cmd: &str,
                     _response: &InvokeResponse,
                     _callback: CallbackFn,
                     _error: CallbackFn| {};

    Box::new(responder)
  }

  pub fn initialization_script(&self) -> String {
    format!(
      "
      let globalId = 0;
      Object.defineProperty(window.__TAURI_INTERNALS__, 'postMessage', {{
        value: (message) => {{
          const ws = new WebSocket('ws://localhost:{}', \"json\");
          const rpcMethodId = globalId;
          globalId += 1;

          ws.onmessage = function (event) {{
            let rpcMessage = JSON.parse(event.data);

            if (rpcMessage.id === rpcMethodId) {{
              if ([\"Invalid\", \"Error\"].includes(rpcMessage.result.status)) {{
                window[`_${{message.error}}`](rpcMessage.result.data);
                delete window[`_${{message.error}}`];
                ws.close();
              }}

              if (rpcMessage.result.status === \"Success\") {{
                window[`_${{message.callback}}`](rpcMessage.result.data);
                delete window[`_${{message.callback}}`];
                ws.close();
              }}
            }}
          }};

          ws.onerror = (e) => {{
            ws.close();
            window[`_${{message.error}}`](e)
            delete window[`_${{message.error}}`];
          }};


          ws.onopen = () => {{
            ws.send(
              JSON.stringify({{
                jsonrpc: \"2.0\",
                id: rpcMethodId,
                method: \"invoke\",
                params: {{id: rpcMethodId, window_label: window.__TAURI_INTERNALS__.metadata.currentWindow.label, payload: JSON.stringify(message) }},
              }})
            );
          }};
        }}
      }});


      Object.defineProperty(window, 'AwesomeEvent', {{
        value: {{
          listen: (event_name, callback) => {{
            const ws = new WebSocket('ws://localhost:{}', \"json\");
            ws.onmessage = function (event) {{
              let message = JSON.parse(event.data);

              if (message.event_name && message.event_name === event_name && [null, window.__TAURI_INTERNALS__.metadata.currentWindow].includes(message.window_label)) {{
                callback(message.payload);
              }}
            }};

            ws.onerror = (e) => {{
              ws.close();
            }};

            return () => ws.close();
          }}
        }}
      }})
    ",
      self.port, self.port
    )
  }
}

#[derive(Serialize)]
struct AwesomeEvent<P> {
  event_name: String,
  window_label: Option<String>,
  payload: P,
}

#[derive(Clone)]
pub struct AwesomeEmit {
  broadcaster: Broadcaster,
}

impl AwesomeEmit {
  pub fn new(broadcaster: Broadcaster) -> Self {
    Self { broadcaster }
  }

  pub fn send<P: Serialize>(&self, payload: P) {
    self
      .broadcaster
      .send(serde_json::to_string(&payload).unwrap())
      .unwrap();
  }

  #[allow(dead_code)]
  pub fn emit_all<P: Serialize>(&self, name: &str, payload: P) {
    self
      .broadcaster
      .send(
        serde_json::to_string(&AwesomeEvent {
          event_name: name.into(),
          window_label: None,
          payload,
        })
        .unwrap(),
      )
      .unwrap();
  }

  #[allow(dead_code)]
  pub fn emit<P: Serialize>(&self, window_label: &str, name: &str, payload: P) {
    self
      .broadcaster
      .send(
        serde_json::to_string(&AwesomeEvent {
          event_name: name.into(),
          window_label: Some(window_label.into()),
          payload,
        })
        .unwrap(),
      )
      .unwrap();
  }
}
