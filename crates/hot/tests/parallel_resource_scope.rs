//! Execute real Hot parallel/pmap branches under two concurrent bundle scopes.
//! This dedicated integration binary owns the process-global host registry, so
//! the negative control cannot race with resource tests in other binaries.
use hot::lang::compiler::{Compiler, core_registry::CoreVariableRegistry};
use hot::lang::hot::resource::{self, ResourceRegistry, ThreadRegistryGuard};
use hot::lang::parser::parse_hot;
use hot::lang::runtime::vm::VirtualMachine;
use hot::val::Val;
use std::path::Path;
use std::sync::{Arc, Barrier};

const SOURCE: &str = r#"
::resource_scope ns

load-one fn (unused) { call-lib(::hot::resource/load-str, ["tenant.txt"]) }
host-exists fn (unused) { call-lib(::hot::resource/exists, ["host-only.txt"]) }

parallel-load fn () {
    values parallel {
        first call-lib(::hot::resource/load-str, ["tenant.txt"])
        second call-lib(::hot::resource/load-str, ["tenant.txt"])
        host call-lib(::hot::resource/exists, ["host-only.txt"])
    }
    [values.first, values.second, values.host]
}

mapped-load fn () {
    call-lib(::hot::coll/pmap, [[0, 1, 2, 3, 4, 5, 6, 7], load-one])
}

mapped-host fn () {
    call-lib(::hot::coll/pmap, [[0, 1, 2, 3, 4, 5, 6, 7], host-exists])
}

nested-load fn () {
    values parallel {
        first mapped-load()
        second mapped-load()
    }
    [values.first, values.second]
}

parallel-host fn () {
    values parallel {
        first call-lib(::hot::resource/exists, ["host-only.txt"])
        second call-lib(::hot::resource/exists, ["host-only.txt"])
    }
    [values.first, values.second]
}
"#;

fn vm() -> VirtualMachine {
    let mut program = parse_hot(SOURCE).expect("resource regression source should parse");
    let mut compiler = Compiler::new();
    compiler.compile_program_unchecked(&mut program).unwrap();
    let mut vm = VirtualMachine::new(
        compiler.get_program_arc(),
        None,
        compiler.get_function_mapping_arc(),
        compiler.get_core_functions_arc(),
        compiler.get_type_implementations_arc(),
        Arc::new(CoreVariableRegistry::new()),
        Some(hot::val!({"engine":{"threads":2},"jit":"off"})),
    );
    vm.execute().unwrap();
    assert_eq!(vm.get_thread_count(), 2);
    vm
}

fn call(vm: &mut VirtualMachine, name: &str) -> Val {
    let value = vm
        .call_function_bypassing_unified_lookup(&format!("::resource_scope/{name}"), &[])
        .unwrap_or_else(|error| panic!("{name} failed: {error}"));
    value.unwrap_ok().cloned().unwrap_or(value)
}

fn bundle(root: &Path, content: &str) -> Arc<ResourceRegistry> {
    std::fs::create_dir_all(root.join("resources")).unwrap();
    std::fs::write(root.join("resources/tenant.txt"), content).unwrap();
    Arc::new(resource::build_registry_from_manifest(
        &Val::Vec(vec![hot::val!({"path":"tenant.txt"})]),
        root,
    ))
}

#[test]
fn parallel_and_pmap_workers_inherit_bundle_resource_authority() {
    let temp = tempfile::tempdir().unwrap();
    let host = temp.path().join("host");
    std::fs::create_dir_all(&host).unwrap();
    std::fs::write(host.join("tenant.txt"), "host-secret").unwrap();
    std::fs::write(host.join("host-only.txt"), "private-host-resource").unwrap();
    resource::set_registry(resource::build_registry(&[host], false, &[]));
    let tenants = [
        (
            bundle(&temp.path().join("alpha"), "alpha-resource"),
            "alpha-resource",
        ),
        (
            bundle(&temp.path().join("beta"), "beta-resource"),
            "beta-resource",
        ),
    ];
    let barrier = Barrier::new(tenants.len());
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (registry, expected) in tenants {
            let barrier = &barrier;
            handles.push(scope.spawn(move || {
                let _guard = ThreadRegistryGuard::install(registry);
                barrier.wait();
                let mut vm = vm();
                assert_eq!(
                    call(&mut vm, "parallel-load"),
                    Val::Vec(vec![
                        Val::from(expected),
                        Val::from(expected),
                        Val::Bool(false)
                    ]),
                );
                let mapped = Val::Vec(vec![Val::from(expected); 8]);
                assert_eq!(call(&mut vm, "mapped-load"), mapped);
                assert_eq!(
                    call(&mut vm, "mapped-host"),
                    Val::Vec(vec![Val::Bool(false); 8])
                );
                assert_eq!(
                    call(&mut vm, "nested-load"),
                    Val::Vec(vec![mapped.clone(), mapped])
                );
                // Empty bundle scopes still deny host resources in every worker.
                let _empty = ThreadRegistryGuard::install(Arc::new(ResourceRegistry::default()));
                assert_eq!(
                    call(&mut vm, "parallel-host"),
                    Val::Vec(vec![Val::Bool(false); 2])
                );
                assert_eq!(
                    call(&mut vm, "mapped-host"),
                    Val::Vec(vec![Val::Bool(false); 8])
                );
            }));
        }
        for handle in handles {
            handle
                .join()
                .expect("tenant execution should stay isolated");
        }
    });
    // Positive control: the process-global resources really are available when
    // no bundle scope is installed. Nested scopes never replaced this registry.
    let mut host_vm = vm();
    assert_eq!(
        call(&mut host_vm, "mapped-host"),
        Val::Vec(vec![Val::Bool(true); 8])
    );
    assert_eq!(
        call(&mut host_vm, "mapped-load"),
        Val::Vec(vec![Val::from("host-secret"); 8])
    );
}
