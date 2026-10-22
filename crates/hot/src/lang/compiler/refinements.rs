//! `where` constraints at compile time.
//!
//! The parser declares each `where` predicate as a hidden var whose value is
//! `::hot::type/predicate(<predicate>)`. Placeholder lowering treats that
//! call's argument as a function slot, so a `%` predicate becomes a lambda
//! there under the ordinary `%` rule, `nested-placeholder` included. This pass
//! then turns each such var into an ordinary one-parameter function, which the
//! resolver, type checker, call graph and emitter handle like any other.

use std::path::PathBuf;

use crate::lang::ast::{FnDef, NsPath, Program, Ref, Source, Value, is_where_predicate};
use crate::lang::errors::{CompilerError, ErrorLocation};

/// True for a call to `::hot::type/predicate`, the wrapper the parser puts
/// around every `where` predicate.
pub(crate) fn is_predicate_call(function: &Value) -> bool {
    matches!(
        function,
        Value::Ref(Ref::Ns(ns_ref))
            if ns_ref.function_name.as_deref() == Some("predicate")
                && ns_ref.ns == NsPath::from_string("::hot::type")
    )
}

/// Turn each hidden `where` predicate declaration into a one-parameter
/// function. Runs after placeholder lowering.
pub fn lower_predicates(program: &mut Program, errors: &mut Vec<CompilerError>) {
    for namespace in program.namespaces.values_mut() {
        for (var, value) in namespace.scope.vars.iter_mut() {
            if !is_where_predicate(var.sym.name()) {
                continue;
            }
            let Value::FnCall(call) = value else {
                continue;
            };
            if !is_predicate_call(&call.function) || call.args.len() != 1 {
                continue;
            }
            match std::mem::replace(&mut call.args[0].value, Value::Val(crate::val::Val::Null, None)) {
                Value::Lambda(lambda) if !lambda.args.variadic && lambda.args.args.len() == 1 => {
                    *value = Value::Fn(vec![FnDef {
                        args: lambda.args,
                        body: *lambda.body,
                        return_type: None,
                        precondition: None,
                    }]);
                }
                _ => errors.push(CompilerError::Constraint {
                    code: "invalid-constraint",
                    message: "A `where` predicate must be a function of one value".to_string(),
                    help: Some(
                        "use `%`, as in `where gt(%, 0)`, or a lambda, as in `where (x) { gt(x, 0) }`"
                            .to_string(),
                    ),
                    warning: false,
                    location: var.src.as_ref().map(error_location),
                }),
            }
        }
    }
}

pub(crate) fn error_location(src: &Source) -> ErrorLocation {
    ErrorLocation {
        line: src.line,
        column: src.column,
        position: src.position,
        length: src.length,
        file: src.file.as_ref().map(PathBuf::from),
    }
}
