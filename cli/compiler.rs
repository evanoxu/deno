// Copyright 2018-2019 the Deno authors. All rights reserved. MIT license.
use core::ops::Deref;
use crate::flags::DenoFlags;
use crate::isolate_state::*;
use crate::js_errors::JSErrorColor;
use crate::msg;
use crate::ops;
use crate::resources;
use crate::resources::ResourceId;
use crate::startup_data;
use crate::workers;
use crate::workers::WorkerBehavior;
use crate::workers::WorkerInit;
use deno::deno_buf;
use deno::Behavior;
use deno::Buf;
use deno::JSError;
use deno::Op;
use deno::StartupData;
use futures::future::*;
use futures::sync::oneshot;
use futures::Future;
use serde_json;
use std::str;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::runtime::Runtime;

/// Used for normalization of types on internal future completions
type CompilerInnerResult = Result<ModuleMetaData, Option<JSError>>;
type WorkerErrReceiver = oneshot::Receiver<CompilerInnerResult>;

/// Shared resources for used to complete compiler operations.
/// rid is the resource id for compiler worker resource used for sending it
/// compile requests
/// worker_err_receiver is a shared future that will compelete when the
/// compiler worker future completes, and send back an error if present
/// or a None if not
#[derive(Clone)]
struct CompilerShared {
  pub rid: ResourceId,
  pub worker_err_receiver: Shared<WorkerErrReceiver>,
}

lazy_static! {
  // Shared worker resources so we can spawn
  static ref C_SHARED: Mutex<Option<CompilerShared>> = Mutex::new(None);
  // tokio runtime specifically for spawning logic that is dependent on
  // completetion of the compiler worker future
  static ref C_RUNTIME: Mutex<Runtime> = Mutex::new(Runtime::new().unwrap());
}

pub struct CompilerBehavior {
  pub state: Arc<IsolateState>,
}

impl CompilerBehavior {
  pub fn new(flags: DenoFlags, argv_rest: Vec<String>) -> Self {
    Self {
      state: Arc::new(IsolateState::new(flags, argv_rest, None, true)),
    }
  }
}

impl IsolateStateContainer for CompilerBehavior {
  fn state(&self) -> Arc<IsolateState> {
    self.state.clone()
  }
}

impl IsolateStateContainer for &CompilerBehavior {
  fn state(&self) -> Arc<IsolateState> {
    self.state.clone()
  }
}

impl Behavior for CompilerBehavior {
  fn startup_data(&mut self) -> Option<StartupData> {
    Some(startup_data::compiler_isolate_init())
  }

  fn dispatch(
    &mut self,
    control: &[u8],
    zero_copy: deno_buf,
  ) -> (bool, Box<Op>) {
    ops::dispatch_all(self, control, zero_copy, ops::op_selector_compiler)
  }
}

impl WorkerBehavior for CompilerBehavior {
  fn set_internal_channels(&mut self, worker_channels: WorkerChannels) {
    self.state = Arc::new(IsolateState::new(
      self.state.flags.clone(),
      self.state.argv.clone(),
      Some(worker_channels),
      true,
    ));
  }
}

// This corresponds to JS ModuleMetaData.
// TODO Rename one or the other so they correspond.
#[derive(Debug, Clone)]
pub struct ModuleMetaData {
  pub module_name: String,
  pub module_redirect_source_name: Option<String>, // source of redirect
  pub filename: String,
  pub media_type: msg::MediaType,
  pub source_code: Vec<u8>,
  pub maybe_output_code_filename: Option<String>,
  pub maybe_output_code: Option<Vec<u8>>,
  pub maybe_source_map_filename: Option<String>,
  pub maybe_source_map: Option<Vec<u8>>,
}

impl ModuleMetaData {
  pub fn has_output_code_and_source_map(&self) -> bool {
    self.maybe_output_code.is_some() && self.maybe_source_map.is_some()
  }

  pub fn js_source(&self) -> String {
    if self.media_type == msg::MediaType::Json {
      return format!(
        "export default {};",
        str::from_utf8(&self.source_code).unwrap()
      );
    }
    match self.maybe_output_code {
      None => str::from_utf8(&self.source_code).unwrap().to_string(),
      Some(ref output_code) => str::from_utf8(output_code).unwrap().to_string(),
    }
  }
}

fn lazy_start(parent_state: Arc<IsolateState>) -> CompilerShared {
  let mut cell = C_SHARED.lock().unwrap();
  cell
    .get_or_insert_with(|| {
      let worker_result = workers::spawn(
        CompilerBehavior::new(
          parent_state.flags.clone(),
          parent_state.argv.clone(),
        ),
        "TS",
        WorkerInit::Script("compilerMain()".to_string()),
      );
      match worker_result {
        Ok(worker) => {
          let rid = worker.resource.rid;
          // create oneshot channels and use the sender to pass back
          // results from worker future
          let (err_sender, err_receiver) =
            oneshot::channel::<CompilerInnerResult>();
          let mut runtime = C_RUNTIME.lock().unwrap();
          runtime.spawn(lazy(move || {
            let resource = worker.resource.clone();
            worker.then(move |result| -> Result<(), ()> {
              resource.close();
              match result {
                Err(err) => err_sender.send(Err(Some(err))).unwrap(),
                _ => err_sender.send(Err(None)).unwrap(),
              };
              Ok(())
            })
          }));
          CompilerShared {
            rid,
            worker_err_receiver: err_receiver.shared(),
          }
        }
        Err(err) => {
          println!("{}", err.to_string());
          std::process::exit(1);
        }
      }
    }).clone()
}

fn show_compiler_error(err: JSError) -> ModuleMetaData {
  eprintln!("{}", JSErrorColor(&err).to_string());
  std::process::exit(1);
}

fn req(specifier: &str, referrer: &str, is_worker_main: bool) -> Buf {
  json!({
    "specifier": specifier,
    "referrer": referrer,
    "isWorker": is_worker_main
  }).to_string()
  .into_boxed_str()
  .into_boxed_bytes()
}

pub fn compile_sync(
  parent_state: Arc<IsolateState>,
  specifier: &str,
  referrer: &str,
  module_meta_data: &ModuleMetaData,
) -> ModuleMetaData {
  debug!(
    "Running rust part of compile_sync. specifier: {}, referrer: {}",
    &specifier, &referrer
  );

  let req_msg = req(specifier, referrer, parent_state.is_worker);
  let module_meta_data_ = module_meta_data.clone();

  let shared = lazy_start(parent_state);
  let compiler_rid = shared.rid;

  let (local_sender, local_receiver) =
    oneshot::channel::<Result<ModuleMetaData, Option<JSError>>>();

  let mut runtime = C_RUNTIME.lock().unwrap();
  runtime.spawn(lazy(move || {
    resources::post_message_to_worker(compiler_rid, req_msg)
      .then(move |_| {
        debug!("Sent message to worker");
        resources::get_message_from_worker(compiler_rid)
      }).and_then(move |res_msg| {
        debug!("Received message from worker");
        let res_json = std::str::from_utf8(res_msg.as_ref().unwrap()).unwrap();
        let res_data = serde_json::from_str::<serde_json::Value>(res_json)
          .expect("Error decoding compiler response");
        let res_module_meta_data = ModuleMetaData {
          maybe_output_code: res_data["outputCode"]
            .as_str()
            .map(|s| s.as_bytes().to_owned()),
          maybe_source_map: res_data["sourceMap"]
            .as_str()
            .map(|s| s.as_bytes().to_owned()),
          ..module_meta_data_
        };
        Ok(res_module_meta_data)
      }).map_err(|_| None)
      .then(move |result| {
        local_sender.send(result).expect("Oneshot send() failed");
        Ok(())
      })
  }));

  let worker_receiver = shared.worker_err_receiver.clone();

  let union =
    futures::future::select_all(vec![worker_receiver, local_receiver.shared()]);

  match union.wait() {
    Ok((result, i, rest)) => {
      // We got a sucessful finish before any recivers where canceled
      let mut rest_mut = rest;
      match ((*result.deref()).clone(), i) {
        // Either receiver was completed with success.
        (Ok(v), _) => v,
        // Either receiver was completed with a valid error
        // this should be fatal for now since it is not intended
        // to be possible to recover from a uncaught error in a isolate
        (Err(Some(err)), _) => show_compiler_error(err),
        // local_receiver finished first with a none error. This is intended
        // to catch when the local logic can't complete because it is unable
        // to send and/or receive messages from the compiler worker.
        // Due to the way that scheduling works it is very likely that the
        // compiler worker future has already or will in the near future
        // complete with a valid JSError or a None.
        (Err(None), 1) => {
          debug!("Compiler local exited with None error!");
          // While technically possible to get stuck here indefinately
          // in theory it is highly unlikely.
          debug!(
            "Waiting on compiler worker result specifier: {} referrer: {}!",
            specifier, referrer
          );
          let worker_result =
            (*rest_mut.remove(0).wait().unwrap().deref()).clone();
          debug!(
            "Finished waiting on worker result specifier: {} referrer: {}!",
            specifier, referrer
          );
          match worker_result {
            Err(Some(err)) => show_compiler_error(err),
            Err(None) => panic!("Compiler exit for an unknown reason!"),
            Ok(v) => v,
          }
        }
        // While possible beccause the compiler worker can exit without error
        // this shouldn't occurr normally and I don't intend to attempt to
        // handle it right now
        (_, i) => panic!("Odd compiler result for future {}!", i),
      }
    }
    // This should always a result of a reciver being cancled
    // in theory but why not give a print out just in case
    Err((err, i, _)) => panic!("compile_sync {} failed: {}", i, err),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tokio_util;

  #[test]
  fn test_compile_sync() {
    tokio_util::init(|| {
      let cwd = std::env::current_dir().unwrap();
      let cwd_string = cwd.to_str().unwrap().to_owned();

      let specifier = "./tests/002_hello.ts";
      let referrer = cwd_string + "/";

      let mut out = ModuleMetaData {
        module_name: "xxx".to_owned(),
        module_redirect_source_name: None,
        filename: "/tests/002_hello.ts".to_owned(),
        media_type: msg::MediaType::TypeScript,
        source_code: include_bytes!("../tests/002_hello.ts").to_vec(),
        maybe_output_code_filename: None,
        maybe_output_code: None,
        maybe_source_map_filename: None,
        maybe_source_map: None,
      };

      out = compile_sync(
        Arc::new(IsolateState::mock()),
        specifier,
        &referrer,
        &out,
      );
      assert!(
        out
          .maybe_output_code
          .unwrap()
          .starts_with("console.log(\"Hello World\");".as_bytes())
      );
    });
  }
}
