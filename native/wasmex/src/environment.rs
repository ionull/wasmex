use crate::{
    atoms,
    caller::{remove_caller, set_caller},
    instance::{map_wasm_values_to_vals, ImportDefinition, LinkedModule, WasmValue},
    memory::MemoryResource,
    store::{StoreData, StoreOrCaller, StoreOrCallerResource},
};
use rustler::{
    types::tuple, Atom, Encoder, Error, ListIterator, LocalPid, MapIterator, OwnedEnv, ResourceArc, Term
};
use std::sync::{Condvar, Mutex};
use wasmtime::{Caller, Engine, FuncType, Linker, Val, ValType};
use wiggle::anyhow::{self, anyhow};

pub struct CallbackTokenResource {
    pub token: CallbackToken,
}

#[rustler::resource_impl()]
impl rustler::Resource for CallbackTokenResource {}

pub struct CallbackToken {
    pub continue_signal: Condvar,
    pub return_types: Vec<ValType>,
    pub return_values: Mutex<Option<(bool, Vec<WasmValue>)>>,
}

pub fn link_modules(
    linker: &mut Linker<StoreData>,
    store: &mut StoreOrCaller,
    linked_modules: Vec<LinkedModule>,
) -> Result<(), Error> {
    for linked_module in linked_modules {
        let module_name = linked_module.name;
        let module = linked_module.module_resource.inner.lock().map_err(|e| {
            rustler::Error::Term(Box::new(format!(
                "Could not unlock linked module resource as the mutex was poisoned: {e}"
            )))
        })?;

        let instance = linker.instantiate(&mut *store, &module).map_err(|e| {
            rustler::Error::Term(Box::new(format!(
                "Could not instantiate linked module: {e}"
            )))
        })?;

        linker
            .instance(&mut *store, &module_name, instance)
            .map_err(|err| Error::Term(Box::new(err.to_string())))?;
    }

    Ok(())
}

pub fn imports_from_map_iterator(
    imports: MapIterator,
) -> Result<Vec<ImportDefinition>, Error> {
    let mut result = Vec::new();
    for (namespace_name, namespace_definition) in imports {
        let namespace_name = namespace_name.decode::<String>()?;
        let definition: MapIterator = namespace_definition.decode()?;

        for (import_name, import) in definition {
            let import_name = import_name.decode::<String>()?;
            let import_tuple = tuple::get_tuple(import)?;

            let import_type = import_tuple
                .first()
                .ok_or(Error::Atom("missing_import_type"))?;
            let import_type =
                Atom::from_term(*import_type).map_err(|_| Error::Atom("import type must be an atom"))?;

            if atoms::__fn__().eq(&import_type) {
                let param_term = import_tuple
                    .get(1)
                    .ok_or(Error::Atom("missing_import_params"))?;
                let results_term = import_tuple
                    .get(2)
                    .ok_or(Error::Atom("missing_import_results"))?;

                let params_signature = param_term
                    .decode::<ListIterator>()?
                    .map(term_to_arg_type)
                    .collect::<Result<Vec<ValType>, _>>()?;

                let results_signature = results_term
                    .decode::<ListIterator>()?
                    .map(term_to_arg_type)
                    .collect::<Result<Vec<ValType>, _>>()?;

                result.push(ImportDefinition::Function {
                    namespace: namespace_name.clone(),
                    name: import_name.clone(),
                    params: params_signature,
                    results: results_signature,
                });
            } else {
                return Err(Error::Atom("unknown import type"));
            }
        }
    }
    Ok(result)
}

pub fn link_imports(
    engine: &Engine,
    linker: &mut Linker<StoreData>,
    imports: &Vec<ImportDefinition>,
    pid: &LocalPid,
) -> Result<(), Error> {
    for import_definition in imports {
        link_import(engine, linker, import_definition, pid)?;
    }
    Ok(())
}

fn link_import(
    engine: &Engine,
    linker: &mut Linker<StoreData>,
    import_definition: &ImportDefinition,
    pid: &LocalPid,
) -> Result<(), Error> {
    match import_definition {
        ImportDefinition::Function {
            namespace,
            name,
            params,
            results,
        } => link_imported_function(engine, linker, namespace, name, params, results, pid),
    }
}

// Creates a wrapper function used in a Wasm import object.
//
// The `definition` term must contain a function signature matching the signature of the Wasm import.
// Once the imported function is called during Wasm execution, the following happens:
//
// 1. the rust wrapper we define here is called
// 2. it creates a callback token containing a Mutex for storing the call result and a Condvar
// 3. the rust wrapper sends an :invoke_callback message to elixir containing the token and call params
// 4. the Wasmex module receive that call in elixir-land and executes the actual elixir callback
// 5. after the callback finished execution, return values are send back to Rust via `receive_callback_result`
// 6. `receive_callback_result` saves the return values in the callback tokens mutex and signals the condvar,
//    so that the original wrapper function can continue code execution
fn link_imported_function(
    engine: &Engine,
    linker: &mut Linker<StoreData>,
    namespace_name: &String,
    import_name: &String,
    params_signature: &Vec<ValType>,
    results_signature: &Vec<ValType>,
    pid: &LocalPid,
) -> Result<(), Error> {
    let namespace_name = namespace_name.clone();
    let import_name = import_name.clone();
    let params_signature = params_signature.clone();
    let results_signature = results_signature.clone();
    let signature = FuncType::new(engine, params_signature.clone(), results_signature.clone());
    let pid = pid.clone();

    linker
        .func_new(
            &namespace_name.clone(),
            &import_name.clone(),
            signature,
            move |mut caller: Caller<'_, StoreData>,
                  params: &[Val],
                  results: &mut [Val]|
                  -> Result<(), anyhow::Error> {
                let callback_token = ResourceArc::new(CallbackTokenResource {
                    token: CallbackToken {
                        continue_signal: Condvar::new(),
                        return_types: results_signature.clone(),
                        return_values: Mutex::new(None),
                    },
                });

                let memory = caller
                    .get_export("memory")
                    .and_then(|memory| memory.into_memory());

                let caller_token = set_caller(caller);

                let mut msg_env = OwnedEnv::new();
                let result = msg_env.send_and_clear(&pid.clone(), |env| {
                    let mut callback_params: Vec<Term> = Vec::with_capacity(params.len());
                    for value in params {
                        callback_params.push(match value {
                            Val::I32(i) => i.encode(env),
                            Val::I64(i) => i.encode(env),
                            Val::F32(i) => f32::from_bits(*i).encode(env),
                            Val::F64(i) => f64::from_bits(*i).encode(env),
                            Val::V128(i) => i.as_u128().encode(env),
                            Val::ExternRef(_) => {
                                (atoms::error(), "unable_to_convert_extern_ref_type").encode(env)
                            }
                            Val::FuncRef(_) => {
                                (atoms::error(), "unable_to_convert_func_ref_type").encode(env)
                            }
                            Val::AnyRef(_) => {
                                (atoms::error(), "unable_to_convert_any_ref_type").encode(env)
                            }
                        })
                    }
                    // Callback context will contain memory (plus maybe globals, tables etc later).
                    // This will allow Elixir callback to operate on these objects.
                    let callback_context = Term::map_new(env);

                    let memory = memory.map(|memory| {
                        ResourceArc::new(MemoryResource {
                            inner: Mutex::new(memory),
                        })
                    });
                    let callback_context = Term::map_put(
                        callback_context,
                        atoms::memory().encode(env),
                        memory.encode(env),
                    )
                    .unwrap();

                    let caller_resource = ResourceArc::new(StoreOrCallerResource {
                        inner: Mutex::new(StoreOrCaller::Caller(caller_token)),
                    });

                    let callback_context = Term::map_put(
                        callback_context,
                        atoms::caller().encode(env),
                        caller_resource.encode(env),
                    )
                    .unwrap();
                    (
                        atoms::invoke_callback(),
                        namespace_name.clone(),
                        import_name.clone(),
                        callback_context,
                        callback_params,
                        callback_token.clone(),
                    )
                        .encode(env)
                });

                result.expect("expect no send error");

                // Wait for the thread to start up - `receive_callback_result` is responsible for that.
                let mut result = callback_token.token.return_values.lock().unwrap();
                while result.is_none() {
                    result = callback_token.token.continue_signal.wait(result).unwrap();
                }
                remove_caller(caller_token);

                let result: &(bool, Vec<WasmValue>) = result
                    .as_ref()
                    .expect("expect callback token to contain a result");
                match result {
                    (true, return_values) => write_results(results, return_values),
                    (false, _) => Err(anyhow!("the elixir callback threw an exception")),
                }
            },
        )
        .map_err(|err| Error::Term(Box::new(err.to_string())))?;

    Ok(())
}

fn write_results(results: &mut [Val], return_values: &[WasmValue]) -> Result<(), anyhow::Error> {
    results.clone_from_slice(&map_wasm_values_to_vals(return_values));
    Ok(())
}

fn term_to_arg_type(term: Term) -> Result<ValType, Error> {
    match Atom::from_term(term) {
        Ok(atom) => {
            if atoms::i32().eq(&atom) {
                Ok(ValType::I32)
            } else if atoms::i64().eq(&atom) {
                Ok(ValType::I64)
            } else if atoms::f32().eq(&atom) {
                Ok(ValType::F32)
            } else if atoms::f64().eq(&atom) {
                Ok(ValType::F64)
            } else if atoms::v128().eq(&atom) {
                Ok(ValType::V128)
            } else {
                Err(Error::Atom("unknown"))
            }
        }
        Err(_) => Err(Error::Atom("not_an_atom")),
    }
}
