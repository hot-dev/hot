//! Compiled code's limits and resources: the stack guard and the call-depth
//! cap, self tail calls, closures, code memory, Dec constants, contained
//! helper panics, finalization and call tracing.

use super::*;
use crate::lang::compiler::core_registry::CoreVariableRegistry;
use crate::lang::emitter::{EngineEvent, EngineEventEmitter};
use crate::lang::event::ExecutionContext;
use crate::val;
use std::sync::Mutex;

/// `source` compiled and its namespaces initialized, with `jit` as the VM's
/// JIT config.
fn source_vm(source: &str, jit: Val) -> VirtualMachine {
    source_vm_compiled(source, jit, false)
}

/// `source` compiled, with the compiler's checks when `checked`, and its
/// namespaces initialized, with `jit` as the VM's JIT config.
fn source_vm_compiled(source: &str, jit: Val, checked: bool) -> VirtualMachine {
    let mut program = crate::lang::parser::parse_hot(source).expect("test source parses");
    let mut compiler = crate::lang::compiler::Compiler::new();
    if checked {
        compiler
            .compile_program(&mut program)
            .expect("test source compiles");
    } else {
        compiler
            .compile_program_unchecked(&mut program)
            .expect("test source compiles");
    }
    let mut vm = VirtualMachine::new(
        compiler.get_program_arc(),
        None,
        compiler.get_function_mapping_arc(),
        compiler.get_core_functions_arc(),
        compiler.get_type_implementations_arc(),
        Arc::new(CoreVariableRegistry::new()),
        Some(val!({ "jit": jit })),
    );
    vm.execute().expect("namespace initialization runs");
    vm
}

fn compiled_vm(source: &str) -> VirtualMachine {
    source_vm(source, val!({"mode": "enabled", "threshold": 1}))
}

fn interpreted_vm(source: &str) -> VirtualMachine {
    source_vm(source, val!({"mode": "disabled"}))
}

fn function_id(vm: &VirtualMachine, name: &str) -> FunctionId {
    vm.program
        .functions
        .iter()
        .position(|f| f.name == name)
        .expect("the function exists") as FunctionId
}

fn call(vm: &mut VirtualMachine, name: &str, args: &[Val]) -> Result<Val, String> {
    vm.call_function_bypassing_unified_lookup(name, args)
        .map_err(|err| err.to_string())
}

/// Run `f` on a thread with the stack the VM's threads get.
fn on_vm_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(VM_THREAD_STACK_SIZE)
        .spawn(f)
        .expect("the thread starts")
        .join()
        .expect("the thread finishes")
}

/// Run `f` once the stack pointer is within `margin` bytes above `limit`.
fn near_stack_limit<T>(limit: u64, margin: u64, f: &mut dyn FnMut() -> T) -> T {
    let pad = [0u8; 4096];
    std::hint::black_box(&pad);
    let anchor = 0u64;
    let sp = std::ptr::addr_of!(anchor) as u64;
    let result = if sp > limit + margin {
        near_stack_limit(limit, margin, f)
    } else {
        f()
    };
    std::hint::black_box(&pad);
    result
}

/// `::t/sum-down(n)`: `n + sum-down(n - 1)`, down to 0, as a self-call by
/// function id, which compiled code makes natively.
fn sum_down_vm(jit: Val) -> VirtualMachine {
    let mut program = BytecodeProgram::new();
    let one = program.constants.len() as u32;
    program.constants.push(Constant::Val(val!(1)));
    let zero = program.constants.len() as u32;
    program.constants.push(Constant::Val(val!(0)));
    let n = program.constants.len() as u32;
    program.constants.push(Constant::Val(val!("n")));
    program.functions.push(FunctionInfo {
        name: "::t/sum-down".to_string(),
        namespace: "::t".to_string(),
        arity: 1,
        is_variadic: false,
        param_names: vec!["n".to_string()],
        param_types: vec![],
        return_type: 0,
        lazy_params: vec![false],
        flow_type: None,
        instructions: vec![
            Instruction::LoadVar {
                dest: 0,
                var_name: n,
            },
            Instruction::LoadConst {
                dest: 1,
                constant: one,
            },
            Instruction::Lt {
                dest: 2,
                left: 0,
                right: 1,
            },
            Instruction::JumpIfNot {
                condition: 2,
                offset: 6,
            },
            Instruction::LoadConst {
                dest: 3,
                constant: zero,
            },
            Instruction::Return { value: 3 },
            Instruction::LoadVar {
                dest: 4,
                var_name: n,
            },
            Instruction::LoadVar {
                dest: 5,
                var_name: n,
            },
            Instruction::LoadConst {
                dest: 6,
                constant: one,
            },
            Instruction::Sub {
                dest: 7,
                left: 5,
                right: 6,
            },
            Instruction::CallUserFunction {
                dest: 8,
                function_id: 0,
                args_start: 7,
                args_count: 1,
            },
            Instruction::Add {
                dest: 9,
                left: 4,
                right: 8,
            },
            Instruction::Return { value: 9 },
        ],
        register_count: 10,
        source: None,
    });
    VirtualMachine::new(
        Arc::new(program),
        None,
        Arc::new(indexmap::IndexMap::<String, FunctionId>::new()),
        Arc::new(indexmap::IndexMap::<String, FunctionId>::new()),
        Arc::new(indexmap::IndexMap::<(String, String), String>::new()),
        Arc::new(CoreVariableRegistry::new()),
        Some(val!({ "jit": jit })),
    )
}

fn sum_down(vm: &mut VirtualMachine, n: i64) -> Result<Val, String> {
    vm.execute_compiled_user_function(0, &[Val::Int(n)])
        .map_err(|err| err.to_string())
}

const DEPTH_SOURCE: &str = r#"
::t ns

depth fn (n: Int): Int {
    cond {
        eq(n, 0) => { 0 }
        => { add(1, depth(sub(n, 1))) }
    }
}
"#;

// Compiled self-recursion went on past `HOT_MAX_RECURSION_DEPTH`, which the
// interpreter enforces, so whether a call failed depended on the JIT.
#[test]
fn compiled_self_recursion_stops_at_the_depth_cap() {
    on_vm_thread(|| {
        let max = max_recursion_depth() as i64;
        // The deepest call either tier makes: one frame per n, and 0.
        let deepest = max - 1;
        let mut compiled = sum_down_vm(val!({"mode": "enabled", "threshold": 1}));
        assert_eq!(
            sum_down(&mut compiled, deepest),
            Ok(Val::Int(deepest * (deepest + 1) / 2))
        );
        assert!(compiled.jit_has_compiled_function(0));
        let err = sum_down(&mut compiled, max).expect_err("one frame more fails");
        assert!(err.contains("recursion limit reached"), "{err}");
        // The VM goes on after the refused call.
        assert_eq!(sum_down(&mut compiled, 3), Ok(val!(6)));
        // The interpreter refuses the same depth.
        let mut interpreted = interpreted_vm(DEPTH_SOURCE);
        assert_eq!(
            call(&mut interpreted, "::t/depth", &[Val::Int(deepest)]),
            Ok(Val::Int(deepest))
        );
        let err = call(&mut interpreted, "::t/depth", &[Val::Int(max)])
            .expect_err("one frame more fails");
        assert!(err.contains("recursion limit reached"), "{err}");
    });
}

// A stack guard anchored where its thread first ran compiled code sat below
// the end of the stack when that first call was already deep, so deep
// compiled recursion overflowed the stack and aborted the process.
#[test]
fn jit_stack_limit_from_a_deep_first_call_stays_in_the_stack() {
    on_vm_thread(|| {
        let anchor = 0u64;
        let top = std::ptr::addr_of!(anchor) as u64;
        // 30 MB down, where the old guard put its limit 60 MB lower still:
        // past the end of a 64 MiB stack.
        let deep = top - 30_000_000;
        near_stack_limit(deep, 0, &mut || {
            let anchor = 0u64;
            let sp = std::ptr::addr_of!(anchor) as u64;
            let limit = jit_stack_limit();
            let bottom = top - VM_THREAD_STACK_SIZE as u64;
            assert!(limit > bottom, "the limit is inside the stack");
            assert!(limit < sp, "the limit is below the deep call");
            assert!(
                limit - bottom <= JIT_STACK_RESERVE + 1024 * 1024,
                "the limit is a reserve's distance from the end of the stack"
            );
        });
    });
}

// A compiled call that ran out of stack deep in its recursion returned the
// "out of stack before starting" result, so the interpreter ran the whole
// call again, repeating every effect it had already had.
#[test]
fn stack_exhaustion_inside_compiled_code_fails_the_call() {
    on_vm_thread(|| {
        let mut vm = sum_down_vm(val!({"mode": "enabled", "threshold": 1}));
        assert_eq!(sum_down(&mut vm, 10), Ok(val!(55)));
        let args = [val!(3000)];
        let compiled = vm
            .jit_state_mut()
            .compiled_function(0, &args)
            .expect("sum-down compiled");
        let limit = jit_stack_limit();
        let prev = set_jit_vm_ptr(&mut vm as *mut VirtualMachine);
        // Room to start, not to recurse 3,000 deep.
        let started = near_stack_limit(limit, 32 * 1024, &mut || compiled.try_call(&args));
        // No room to start: the interpreter runs the call.
        let refused = near_stack_limit(limit, 0, &mut || compiled.try_call(&args));
        // With the stack back, the same code runs the call.
        let rerun = compiled.try_call(&args);
        set_jit_vm_ptr(prev);
        let err = started.expect_err("running out of stack fails the call");
        assert!(err.contains("Native stack budget exhausted"), "{err}");
        assert_eq!(refused, Ok(None));
        assert_eq!(rerun, Ok(Some(val!(4501500))));
    });
}

const TAIL_SOURCE: &str = r#"
::t ns

id fn (x) { x }

total fn (items: Vec, i: Int, acc: Dec): Dec {
    cond {
        eq(i, length(items)) => { acc }
        => { total(items, add(i, 1), add(acc, items[i].price)) }
    }
}

untyped-total fn (items: Vec, i: Int, acc) {
    cond {
        eq(i, length(items)) => { acc }
        => { untyped-total(items, add(i, 1), add(acc, items[i].price)) }
    }
}

flags fn (n: Int, flag: Bool): Bool {
    cond {
        eq(n, 0) => { flag }
        => { flags(sub(n, 1), id(eq(mod(n, 3), 0))) }
    }
}

nulls fn (n: Int, v: Null): Int {
    cond {
        eq(n, 0) => { n }
        => { nulls(sub(n, 1), id(v)) }
    }
}

swap fn (n: Int, a: Dec, b: Dec): Vec {
    cond {
        eq(n, 0) => { [a, b] }
        => { swap(sub(n, 1), b, a) }
    }
}
"#;

// A self tail call whose argument is an owned Dec, Bool or Null (a field
// read, an arithmetic result kept as a value, a call's result) for a
// parameter the loop holds raw was made as a nested call through the VM:
// each iteration nested, so long loops hit the depth cap (or ran a worker
// out of memory). And two Dec parameters swapped by a tail call read each
// other's new value.
#[test]
fn self_tail_calls_with_owned_arguments_loop() {
    on_vm_thread(|| {
        let price = Val::Dec(D256::from(5) / D256::from(2));
        let items = |n: usize| {
            Val::Vec(
                (0..n)
                    .map(|_| {
                        let mut item = indexmap::IndexMap::new();
                        item.insert(val!("price"), price.clone());
                        Val::Map(Box::new(item))
                    })
                    .collect(),
            )
        };
        let zero =
            Val::Dec(D256::from_str("0.00", fastnum::decimal::Context::default()).expect("a Dec"));
        let one = Val::Dec(D256::from(1));
        let two = Val::Dec(D256::from(2));
        // Short calls the interpreter's results are compared with, then
        // loops past the depth cap, which a nested call per iteration fails.
        let calls = |n: usize| -> Vec<(&str, Vec<Val>)> {
            vec![
                ("::t/total", vec![items(n), val!(0), zero.clone()]),
                ("::t/untyped-total", vec![items(n), val!(0), zero.clone()]),
                ("::t/flags", vec![Val::Int(n as i64), val!(false)]),
                ("::t/nulls", vec![Val::Int(n as i64), Val::Null]),
                (
                    "::t/swap",
                    vec![Val::Int(n as i64), one.clone(), two.clone()],
                ),
            ]
        };
        let mut compiled = compiled_vm(TAIL_SOURCE);
        let mut interpreted = interpreted_vm(TAIL_SOURCE);
        for (name, args) in calls(41) {
            let expected = call(&mut interpreted, name, &args).expect("the interpreter runs it");
            assert_eq!(call(&mut compiled, name, &args), Ok(expected), "{name}");
            assert!(
                compiled.jit_has_compiled_function(function_id(&compiled, name)),
                "{name} compiles"
            );
        }
        // An even count, so the swaps end where they started.
        let long = (max_recursion_depth() + 100) & !1;
        let results: Vec<_> = calls(long)
            .into_iter()
            .map(|(name, args)| call(&mut compiled, name, &args))
            .collect();
        let total = Val::Dec(D256::from(long as u64) * D256::from(5) / D256::from(2));
        assert_eq!(
            results,
            vec![
                Ok(total.clone()),
                Ok(total),
                Ok(Val::Bool(false)),
                Ok(val!(0)),
                // An even number of swaps.
                Ok(Val::Vec(vec![one, two])),
            ]
        );
    });
}

// Each closure with captures made by compiled code leaked, with everything
// it captured, for the life of the process.
#[test]
fn compiled_closures_release_their_captures() {
    let source = r#"
::t ns

make fn (label: Str) { (x) { label } }
"#;
    let mut vm = compiled_vm(source);
    let label: Arc<str> = Arc::from("captured");
    for _ in 0..200 {
        let closure =
            call(&mut vm, "::t/make", &[Val::Str(Arc::clone(&label))]).expect("make runs");
        drop(closure);
    }
    assert!(vm.jit_has_compiled_function(function_id(&vm, "::t/make")));
    assert_eq!(Arc::strong_count(&label), 1, "no closure holds the label");
}

// A function the JIT declined with an `unsupported.*` reason was compiled
// again, up to the same bailout, on every call.
#[test]
fn a_declined_signature_is_compiled_once() {
    let source = r#"
::t ns

pick fn (x: Int, lazy y: Int): Int { x }
"#;
    let mut vm = compiled_vm(source);
    let id = function_id(&vm, "::t/pick");
    for _ in 0..20 {
        assert_eq!(call(&mut vm, "::t/pick", &[val!(1), val!(2)]), Ok(val!(1)));
    }
    let profile = vm.jit_function_profile(id).expect("pick is profiled");
    assert!(!profile.do_not_jit, "the reason is retryable");
    assert_eq!(profile.failed_signatures.len(), 1);
    assert!(
        !vm.jit_state_mut()
            .should_compile_now(id, &[val!(1), val!(2)])
    );
}

// Compiled code was never freed: every VM that compiled (one per run, pmap
// chunk and parallel branch) left its code pages behind.
#[test]
fn dropping_compiled_code_frees_its_memory() {
    let freed_before = CODE_MODULES_FREED.with(Cell::get);
    let mut vm = compiled_vm(DEPTH_SOURCE);
    assert_eq!(call(&mut vm, "::t/depth", &[val!(3)]), Ok(val!(3)));
    assert!(vm.jit_has_compiled_function(function_id(&vm, "::t/depth")));
    drop(vm);
    assert!(CODE_MODULES_FREED.with(Cell::get) > freed_before);
}

// A compiled lambda read its Dec constants from the lambda's synthetic
// program, which is dropped once the lambda compiles: later calls read
// freed memory (garbage amounts, or a panic in the arithmetic).
#[test]
fn a_compiled_lambda_owns_its_dec_constants() {
    let mut program = BytecodeProgram::new();
    let price = program.constants.len() as u32;
    program
        .constants
        .push(Constant::Val(Val::Dec(D256::from(5) / D256::from(2))));
    let lambda = LambdaInfo {
        parameters: vec![],
        instructions: vec![
            Instruction::LoadConst {
                dest: 0,
                constant: price,
            },
            Instruction::Return { value: 0 },
        ],
        register_count: 1,
        capture_vars: vec![],
        closure_env: AHashMap::new(),
        defining_namespace: "::test".to_string(),
        is_lazy_param: false,
        used_registers: vec![0],
        structural_hash_cache: Default::default(),
        defining_frame: None,
        result_params: Vec::new(),
    };
    let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
    let mut jit = JitRuntimeState::new(0, &conf);
    let (compiled, args) = jit
        .compiled_lambda(&program, &lambda, &[], &[], CallNames::default())
        .expect("the lambda compiles")
        .expect("the lambda compiles at once");
    assert_eq!(compiled._dec_constants.len(), 1);
    // Reuse the memory the synthetic program freed.
    let churn: Vec<Vec<u8>> = (0..1000).map(|i| vec![0xAB; 16 + i % 64]).collect();
    for _ in 0..10 {
        assert_eq!(
            call_compiled_lambda(&compiled, &args),
            Ok(Some(Val::Dec(D256::from(5) / D256::from(2))))
        );
    }
    drop(churn);
}

// A panic in a helper (in hotlib or VM code it ran) can't unwind through
// compiled frames, so it aborted the process.
#[test]
fn a_helper_panic_is_held_until_the_compiled_code_returns() {
    let held = contain_helper_panic(|| 7, || panic!("helper failed"));
    assert_eq!(held, 7);
    // While it is held, no more of the program runs.
    let ran = Cell::new(false);
    let skipped = contain_helper_panic(
        || 8,
        || {
            ran.set(true);
            0
        },
    );
    assert_eq!(skipped, 8);
    assert!(!ran.get());
    let resumed = std::panic::catch_unwind(resume_helper_panic).expect_err("the panic goes on");
    assert_eq!(resumed.downcast_ref::<&str>(), Some(&"helper failed"));
    // Nothing is held any more.
    assert_eq!(contain_helper_panic(|| 9, || 1), 1);
    resume_helper_panic();
}

#[test]
fn a_panic_in_compiled_arithmetic_fails_the_call_without_aborting() {
    let source = r#"
::t ns

scale fn (x: Int, k: Int): Int { mul(x, k) }
"#;
    let mut vm = compiled_vm(source);
    assert_eq!(call(&mut vm, "::t/scale", &[val!(2), val!(3)]), Ok(val!(6)));
    let id = function_id(&vm, "::t/scale");
    let args = [val!(2), val!(3)];
    let compiled = vm
        .jit_state_mut()
        .compiled_function(id, &args)
        .expect("scale compiled");
    // A helper's panic, as hotlib code raises one.
    let held = contain_helper_panic(|| HALT_SENTINEL, || panic!("hotlib panicked"));
    assert_eq!(held, HALT_SENTINEL);
    let prev = set_jit_vm_ptr(&mut vm as *mut VirtualMachine);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| compiled.call(&args)));
    set_jit_vm_ptr(prev);
    let payload = outcome.expect_err("the held panic goes on from the compiled call");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"hotlib panicked"));
}

// On a Linux AArch64 host that denies membarrier, finalizing compiled code
// panicked, failing every run that reached the JIT threshold.
#[test]
fn a_finalize_failure_declines_to_the_interpreter() {
    let mut vm = compiled_vm(DEPTH_SOURCE);
    FAIL_FINALIZE_FOR_TEST.with(|fail| fail.set(true));
    let results: Vec<_> = (0..3)
        .map(|n| call(&mut vm, "::t/depth", &[Val::Int(n)]))
        .collect();
    FAIL_FINALIZE_FOR_TEST.with(|fail| fail.set(false));
    assert_eq!(results, vec![Ok(val!(0)), Ok(val!(1)), Ok(val!(2))]);
    assert!(!vm.jit_has_compiled_function(function_id(&vm, "::t/depth")));
    let status = vm.jit_code_memory_status();
    assert_eq!(status.availability, JitAvailability::Unsupported);
    assert!(
        status
            .reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with(FINALIZE_BAILOUT))
    );
    // Nothing more compiles on this VM.
    assert_eq!(call(&mut vm, "::t/depth", &[val!(4)]), Ok(val!(4)));
    assert!(!vm.jit_has_compiled_function(function_id(&vm, "::t/depth")));
}

// The JIT compiles functions only up to a size: a 5,000-arm match took
// 2 GB to compile.
#[test]
fn a_function_over_the_size_cap_is_interpreted() {
    let arms = MAX_JIT_FUNCTION_INSTRUCTIONS / 4;
    let mut source = String::from("::t ns\n\npick fn (n: Int): Int {\n    cond {\n");
    for arm in 0..arms {
        source.push_str(&format!("        eq(n, {arm}) => {{ {arm} }}\n"));
    }
    source.push_str("        => { -1 }\n    }\n}\n");
    let mut vm = compiled_vm(&source);
    let id = function_id(&vm, "::t/pick");
    assert!(vm.program.functions[id as usize].instructions.len() > MAX_JIT_FUNCTION_INSTRUCTIONS);
    let last = arms as i64 - 1;
    assert_eq!(
        call(&mut vm, "::t/pick", &[Val::Int(last)]),
        Ok(Val::Int(last))
    );
    assert!(!vm.jit_has_compiled_function(id));
    assert!(vm.jit_function_profile(id).is_some_and(|p| p.do_not_jit));
}

/// An emitter that keeps the functions of the call events it gets.
#[derive(Default)]
struct CallEvents(Mutex<Vec<(String, String)>>);

impl EngineEventEmitter for CallEvents {
    fn emit(&self, event: EngineEvent) {
        if event.event_type.starts_with("call:") {
            let function = match event.event_data.get("function_name") {
                Some(Val::Str(name)) => name.to_string(),
                _ => String::new(),
            };
            self.0
                .lock()
                .expect("events lock")
                .push((event.event_type, function));
        }
    }
}

impl CallEvents {
    fn count(&self, event_type: &str, function: Option<&str>) -> usize {
        self.0
            .lock()
            .expect("events lock")
            .iter()
            .filter(|(kind, name)| kind == event_type && function.is_none_or(|f| name == f))
            .count()
    }
}

fn trace(vm: &mut VirtualMachine) -> Arc<CallEvents> {
    let events = Arc::new(CallEvents::default());
    vm.set_emitter(Arc::clone(&events) as Arc<dyn EngineEventEmitter>);
    vm.set_execution_context(ExecutionContext::new(
        uuid::Uuid::now_v7(),
        uuid::Uuid::now_v7(),
        crate::db::run::RunType::Run.as_id(),
        None,
        None,
        None,
        None,
    ));
    events
}

// Compiled calls emitted no call events, so a function's calls vanished
// from the recorded run once it compiled.
#[test]
fn compiled_calls_are_traced_like_interpreted_ones() {
    let source = r#"
::t ns

inc fn (x: Int): Int { add(x, 1) }

down fn (n: Int): Int {
    cond {
        eq(n, 0) => { inc(0) }
        => { add(1, down(sub(n, 1))) }
    }
}
"#;
    let traced = |jit: Val| {
        let mut vm = source_vm(source, jit);
        let events = trace(&mut vm);
        for x in 0..5 {
            assert_eq!(
                call(&mut vm, "::t/inc", &[Val::Int(x)]),
                Ok(Val::Int(x + 1))
            );
        }
        assert_eq!(call(&mut vm, "::t/down", &[val!(4)]), Ok(val!(5)));
        assert_eq!(
            vm.jit_has_compiled_function(function_id(&vm, "::t/inc")),
            vm.jit_config().is_enabled()
        );
        // Each call that starts stops. (The interpreter also traces the
        // inline `cond` flows, which compiled code doesn't.)
        let starts = events.count("call:start", None);
        let flows = events.count("call:start", Some("<cond>"));
        assert_eq!(events.count("call:stop", None), starts);
        [
            events.count("call:start", Some("::t/inc")),
            events.count("call:start", Some("::t/down")),
            starts - flows,
        ]
    };
    let interpreted = traced(val!({"mode": "disabled"}));
    let compiled = traced(val!({"mode": "enabled", "threshold": 1}));
    assert_eq!(interpreted, [6, 5, 11]);
    assert_eq!(compiled, interpreted);

    // A self-call that compiled code makes natively when calls aren't
    // traced goes through the VM when they are, so each call is traced.
    let fib_source = r#"
::t ns

fib fn cond (n: Int): Int {
    ::hot::cmp/lte(n, 1) => { n }
    => { ::hot::math/add(::t/fib(::hot::math/sub(n, 1)), ::t/fib(::hot::math/sub(n, 2))) }
}
"#;
    let mut untraced =
        source_vm_compiled(fib_source, val!({"mode": "enabled", "threshold": 1}), true);
    assert_eq!(call(&mut untraced, "::t/fib", &[val!(5)]), Ok(val!(5)));
    let fib = function_id(&untraced, "::t/fib");
    assert_eq!(
        untraced.jit_state_mut().compiled_result_kinds(fib),
        vec![JitValueKind::Int],
        "fib calls itself natively"
    );
    let fib_calls = |jit: Val| {
        let mut vm = source_vm_compiled(fib_source, jit, true);
        let events = trace(&mut vm);
        assert_eq!(call(&mut vm, "::t/fib", &[val!(5)]), Ok(val!(5)));
        assert_eq!(call(&mut vm, "::t/fib", &[val!(6)]), Ok(val!(8)));
        assert_eq!(
            vm.jit_has_compiled_function(fib),
            vm.jit_config().is_enabled()
        );
        events.count("call:start", Some("::t/fib"))
    };
    // fib(5) makes 15 calls, fib(6) 25.
    assert_eq!(fib_calls(val!({"mode": "disabled"})), 40);
    assert_eq!(fib_calls(val!({"mode": "enabled", "threshold": 1})), 40);
}
