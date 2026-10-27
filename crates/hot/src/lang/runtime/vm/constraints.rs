//! Runtime checks for `where` constraints: the `CheckConstraint`
//! instruction, and running predicates under a budget and a purity guard.
//!
//! The compiler describes each boundary with a spec map; see
//! `compiler::constraint_specs` for its format. A failed check produces a
//! `Result.Err` holding a `::hot::type/ConstraintError` value. The rejected
//! value itself never appears in the error.

use super::{VirtualMachine, VmError, VmResult};
use crate::val::Val;
use ahash::AHashMap;
use fastnum::D256;

/// Instructions one predicate evaluation may run.
pub(crate) const PREDICATE_FUEL: u64 = 1_000_000;
/// Nested calls one predicate evaluation may make.
pub(crate) const PREDICATE_DEPTH: usize = 256;

/// Present on the VM while a `where` predicate runs: natives must be pure,
/// and instructions and calls draw on a budget. Predicates that check other
/// constraints share the outermost guard.
pub(crate) struct ConstraintGuard {
    fuel: u64,
    depth_base: usize,
    stopped: Option<GuardStop>,
}

#[derive(Clone)]
enum GuardStop {
    Fuel,
    Depth,
    Native(String),
}

/// How a predicate evaluation ended.
enum PredicateOutcome {
    Pass,
    Fail,
    NotBool(String),
    Err(Val),
    Halted(String),
    Native(String),
    Budget(&'static str, i64),
}

/// What failed, with enough context to describe it.
struct Failure {
    kind: FailureKind,
    /// The named refinement or type that failed; empty for an inline one.
    constraint: String,
    /// Predicate source, or a description of the expected values.
    predicate: String,
    location: Val,
    path: Vec<Val>,
    value_type: String,
}

enum FailureKind {
    Violation,
    BaseMismatch { expected: String },
    NotOneOf,
    Evaluation { cause: Val },
    Budget { budget: &'static str, limit: i64 },
}

/// The boundary a check runs at, from its spec.
struct Boundary {
    kind: String,
    function: Option<String>,
    parameter: Option<String>,
    binding: Option<String>,
    declared_type: Option<String>,
}

/// Cached constraint specs of named types, keyed by qualified type name.
#[derive(Default)]
pub(crate) struct ConstraintSpecCache {
    /// Identity of the program the cache was built from: its table address
    /// and length.
    key: (usize, usize),
    specs: AHashMap<String, Val>,
}

fn get<'a>(map: &'a Val, key: &str) -> Option<&'a Val> {
    match map {
        Val::Map(entries) => entries.get(&Val::from(key)),
        _ => None,
    }
}

fn get_str<'a>(map: &'a Val, key: &str) -> Option<&'a str> {
    match get(map, key) {
        Some(Val::Str(s)) => Some(s.as_ref()),
        _ => None,
    }
}

fn short_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// `items[1].price` for a path of `["items", 1, "price"]`.
fn path_text(path: &[Val]) -> String {
    let mut text = String::new();
    for step in path {
        match step {
            Val::Int(i) => text.push_str(&format!("[{i}]")),
            Val::Str(s) => {
                if !text.is_empty() {
                    text.push('.');
                }
                text.push_str(s);
            }
            other => text.push_str(&format!("[{other}]")),
        }
    }
    text
}

fn literal_text(values: &[Val]) -> String {
    values
        .iter()
        .map(|v| match v {
            Val::Str(s) => format!("\"{s}\""),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

impl ConstraintGuard {
    fn new(depth_base: usize) -> Self {
        Self {
            fuel: PREDICATE_FUEL,
            depth_base,
            stopped: None,
        }
    }
}

impl Boundary {
    fn from_spec(spec: &Val) -> Self {
        let field = |key: &str| get_str(spec, key).map(str::to_string);
        Self {
            kind: field("boundary").unwrap_or_else(|| "value".to_string()),
            function: field("function"),
            parameter: field("parameter"),
            binding: field("binding"),
            declared_type: field("declared-type"),
        }
    }

    /// What was checked, as the start of a message.
    fn subject(&self) -> String {
        let function = self.function.as_deref().unwrap_or("a function");
        match self.kind.as_str() {
            "parameter" => format!(
                "parameter `{}` of `{}`",
                self.parameter.as_deref().unwrap_or("?"),
                function
            ),
            "arguments" => format!("arguments of `{function}`"),
            "return" => format!("return value of `{function}`"),
            "binding" => format!("`{}`", self.binding.as_deref().unwrap_or("binding")),
            "constructor" => format!(
                "constructor of `{}`",
                short_name(self.declared_type.as_deref().unwrap_or("a type"))
            ),
            "update" => format!(
                "update of `{}`",
                short_name(self.declared_type.as_deref().unwrap_or("a value"))
            ),
            _ => "value".to_string(),
        }
    }
}

impl VirtualMachine {
    /// Charge one instruction to the running predicate's budget.
    #[inline]
    pub(crate) fn charge_constraint_fuel(&mut self) -> VmResult<()> {
        if let Some(guard) = self.constraint.as_mut() {
            if guard.fuel == 0 {
                guard.stopped.get_or_insert(GuardStop::Fuel);
                return Err(VmError::runtime(
                    "A `where` predicate ran out of its instruction budget".to_string(),
                ));
            }
            guard.fuel -= 1;
        }
        Ok(())
    }

    /// Reject a call deeper than the running predicate's depth budget.
    pub(crate) fn check_constraint_depth(&mut self) -> VmResult<()> {
        let depth = self.user_function_depth;
        if let Some(guard) = self.constraint.as_mut()
            && depth > guard.depth_base + PREDICATE_DEPTH
        {
            guard.stopped.get_or_insert(GuardStop::Depth);
            return Err(VmError::runtime(
                "A `where` predicate ran out of its call-depth budget".to_string(),
            ));
        }
        Ok(())
    }

    /// Reject a native that a predicate may not call.
    pub(crate) fn check_constraint_native(&mut self, name: &str) -> VmResult<()> {
        if let Some(guard) = self.constraint.as_mut()
            && !crate::lang::hot::libmap::is_pure_native(name)
        {
            guard
                .stopped
                .get_or_insert_with(|| GuardStop::Native(name.to_string()));
            return Err(VmError::runtime(format!(
                "`{name}` isn't allowed in a `where` predicate"
            )));
        }
        Ok(())
    }

    /// True while a `where` predicate runs. The JIT and parallel execution
    /// stay off then, so every instruction and native call is checked.
    #[inline]
    pub(crate) fn in_constraint_predicate(&self) -> bool {
        self.constraint.is_some()
    }

    /// Execute `CheckConstraint`: check `value` against the boundary `spec`.
    /// Returns the checked value or a `Result.Err` holding a
    /// `ConstraintError`, or a `Bool` when `test` is set. An `Err` passes
    /// through unchanged: the predicate never sees it.
    pub(crate) fn execute_check_constraint(
        &mut self,
        value: Val,
        spec: &Val,
        test: bool,
    ) -> VmResult<Val> {
        if value.is_err() {
            return Ok(if test { Val::Bool(false) } else { value });
        }
        // A parameter whose argument the call site proved: each proof is
        // used once, by the prologue of the call it was made for.
        if let Some(Val::Vec(proof)) = get(spec, "proof")
            && let [Val::Int(function), Val::Int(index)] = proof.as_slice()
            && let Some((id, mask)) = &mut self.entry_proven
            && i64::from(*id) == *function
            && (0..64).contains(index)
            && *mask & (1 << index) != 0
        {
            *mask &= !(1 << index);
            return Ok(value);
        }
        let boundary = Boundary::from_spec(spec);
        let type_spec = get(spec, "type").cloned().unwrap_or(Val::Null);
        let mut path = Vec::new();
        Ok(match self.check_value(value, &type_spec, &mut path)? {
            Ok(checked) => {
                if test {
                    Val::Bool(true)
                } else {
                    checked
                }
            }
            Err(failure) => {
                if test {
                    Val::Bool(false)
                } else {
                    self.constraint_error(&failure, &boundary)
                }
            }
        })
    }

    /// The spec of a constrained named type.
    fn named_constraint_spec(&mut self, name: &str) -> Option<Val> {
        let table = &self.program.constraint_types;
        let key = (table.as_ptr() as usize, table.len());
        if self.constraint_spec_cache.key != key {
            let mut specs = AHashMap::new();
            for (type_name, constant) in table {
                if let Some(crate::lang::bytecode::Constant::Val(spec)) =
                    self.program.constants.get(*constant as usize)
                {
                    specs.insert(type_name.clone(), spec.clone());
                }
            }
            self.constraint_spec_cache = ConstraintSpecCache { key, specs };
        }
        self.constraint_spec_cache.specs.get(name).cloned()
    }

    /// Check the result of a native that updates a typed value (`assoc`,
    /// `merge`, ...) against the constraints of its type.
    pub(crate) fn check_updated_value(
        &mut self,
        native: &str,
        args: &[Val],
        value: Val,
    ) -> VmResult<Val> {
        match native {
            "::hot::coll/assoc"
            | "::hot::coll/assoc-some"
            | "::hot::coll/update"
            | "::hot::coll/merge" => self.check_typed_value(value, "update"),
            // A deep update can change a typed value at any level of its
            // path, not only the outermost one.
            "::hot::coll/assoc-in" | "::hot::coll/update-in" => match args.get(1) {
                Some(Val::Vec(path)) => self.check_typed_values_on_path(value, path, "update"),
                _ => self.check_typed_value(value, "update"),
            },
            // A walk can change a typed value anywhere in the result.
            "::hot::coll/walk"
            | "::hot::coll/prewalk"
            | "::hot::coll/postwalk"
            | "::hot::coll/postwalk-replace" => self.check_typed_values_within(value, "update"),
            _ => Ok(value),
        }
    }

    /// Check the typed values a deep update passed through, from the result
    /// down along `path`; the result is the first `ConstraintError`, or
    /// `value` when all of them pass.
    fn check_typed_values_on_path(
        &mut self,
        value: Val,
        path: &[Val],
        boundary: &str,
    ) -> VmResult<Val> {
        if self.program.constraint_types.is_empty() {
            return Ok(value);
        }
        let mut current = value.clone();
        for key in std::iter::once(None).chain(path.iter().map(Some)) {
            if let Some(key) = key {
                let Some(next) = path_step(&current, key) else {
                    break;
                };
                current = next;
            }
            if let Val::Map(entries) = &current
                && crate::val::tagged_type_name(entries).is_some()
            {
                let checked = self.check_typed_value(current.clone(), boundary)?;
                if checked.is_err() {
                    return Ok(checked);
                }
            }
        }
        Ok(value)
    }

    /// Check every typed value inside `value`, outermost first; the result is
    /// the first `ConstraintError`, or `value` when all of them pass.
    fn check_typed_values_within(&mut self, value: Val, boundary: &str) -> VmResult<Val> {
        if self.program.constraint_types.is_empty() {
            return Ok(value);
        }
        let mut failure = None;
        self.find_typed_value_failure(&value, boundary, &mut failure)?;
        Ok(failure.unwrap_or(value))
    }

    fn find_typed_value_failure(
        &mut self,
        value: &Val,
        boundary: &str,
        failure: &mut Option<Val>,
    ) -> VmResult<()> {
        match value {
            Val::Map(entries) => {
                if crate::val::tagged_type_name(entries).is_some() {
                    let checked = self.check_typed_value(value.clone(), boundary)?;
                    if checked.is_err() {
                        *failure = Some(checked);
                        return Ok(());
                    }
                }
                for nested in entries.values() {
                    self.find_typed_value_failure(nested, boundary, failure)?;
                    if failure.is_some() {
                        break;
                    }
                }
            }
            Val::Vec(items) => {
                for nested in items.iter() {
                    self.find_typed_value_failure(nested, boundary, failure)?;
                    if failure.is_some() {
                        break;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Check a typed value against its type's constraints, at a boundary
    /// such as `update`. Values of unconstrained types pass unchanged.
    pub(crate) fn check_typed_value(&mut self, value: Val, boundary: &str) -> VmResult<Val> {
        let Val::Map(entries) = &value else {
            return Ok(value);
        };
        let Some(type_name) = crate::val::tagged_type_name(entries).map(str::to_string) else {
            return Ok(value);
        };
        if self.named_constraint_spec(&type_name).is_none() {
            return Ok(value);
        }
        let spec = Val::map_from_iter([
            (
                Val::from("type"),
                Val::map_from_iter([
                    (Val::from("kind"), Val::from("named")),
                    (Val::from("type"), Val::from(type_name.as_str())),
                ]),
            ),
            (Val::from("boundary"), Val::from(boundary)),
            (Val::from("declared-type"), Val::from(type_name.as_str())),
        ]);
        self.execute_check_constraint(value, &spec, false)
    }

    /// Whether `value` satisfies a constrained named type, for `is-type`
    /// and `match` arms. `None` when the type has no constraints, so the
    /// ordinary nominal check applies.
    pub(crate) fn satisfies_constrained_type(
        &mut self,
        value: &Val,
        qualified: &str,
    ) -> VmResult<Option<bool>> {
        // Structs and enums keep their nominal tags, which construction and
        // updates already checked; only refinements need the predicate.
        match self.named_constraint_spec(qualified) {
            None => return Ok(None),
            Some(spec) if matches!(get_str(&spec, "kind"), Some("struct" | "enum")) => {
                return Ok(None);
            }
            Some(_) => {}
        }
        let spec = Val::map_from_iter([
            (
                Val::from("type"),
                Val::map_from_iter([
                    (Val::from("kind"), Val::from("named")),
                    (Val::from("type"), Val::from(qualified)),
                ]),
            ),
            (Val::from("boundary"), Val::from("is-type")),
        ]);
        match self.execute_check_constraint(value.clone(), &spec, true)? {
            Val::Bool(satisfied) => Ok(Some(satisfied)),
            _ => Ok(Some(false)),
        }
    }

    fn type_name_of(&self, value: &Val) -> String {
        self.get_val_type_name(value)
    }

    fn failure(&self, kind: FailureKind, value: &Val, path: &[Val]) -> Box<Failure> {
        Box::new(Failure {
            kind,
            constraint: String::new(),
            predicate: String::new(),
            location: Val::Null,
            path: path.to_vec(),
            value_type: self.type_name_of(value),
        })
    }

    /// Check `value` against a spec node. The outer `VmResult` carries VM
    /// errors; the inner result is the checked value or what failed.
    fn check_value(
        &mut self,
        value: Val,
        node: &Val,
        path: &mut Vec<Val>,
    ) -> VmResult<Result<Val, Box<Failure>>> {
        match get_str(node, "kind").unwrap_or("any") {
            "base" => Ok(self.check_base(value, get_str(node, "type").unwrap_or("Any"), path)),
            "literal" => {
                let values = match get(node, "values") {
                    Some(Val::Vec(values)) => values.clone(),
                    _ => Vec::new(),
                };
                if values.iter().any(|v| v == &value) {
                    return Ok(Ok(value));
                }
                let mut failure = self.failure(FailureKind::NotOneOf, &value, path);
                failure.constraint = get_str(node, "type").unwrap_or_default().to_string();
                failure.predicate = literal_text(&values);
                Ok(Err(failure))
            }
            "optional" => {
                if matches!(value, Val::Null) {
                    return Ok(Ok(value));
                }
                let inner = get(node, "inner").cloned().unwrap_or(Val::Null);
                self.check_value(value, &inner, path)
            }
            "union" => {
                let members = match get(node, "members") {
                    Some(Val::Vec(members)) => members.clone(),
                    _ => Vec::new(),
                };
                let mut failures = Vec::new();
                for member in &members {
                    match self.check_value(value.clone(), member, path)? {
                        Ok(checked) => return Ok(Ok(checked)),
                        Err(failure) => failures.push(failure),
                    }
                }
                // Prefer the member whose base type matched: its predicate
                // says more than "expected another type".
                let index = failures
                    .iter()
                    .position(|f| !matches!(f.kind, FailureKind::BaseMismatch { .. }))
                    .unwrap_or(0);
                match failures.into_iter().nth(index) {
                    Some(failure) => Ok(Err(failure)),
                    None => Ok(Ok(value)),
                }
            }
            "vec" => {
                let Val::Vec(items) = value else {
                    return Ok(Err(self.base_mismatch("Vec", &value, path)));
                };
                let elem = get(node, "elem").cloned().unwrap_or(Val::Null);
                let mut checked_items = Vec::with_capacity(items.len());
                for (index, item) in items.into_iter().enumerate() {
                    path.push(Val::Int(index as i64));
                    let result = self.check_value(item, &elem, path)?;
                    path.pop();
                    match result {
                        Ok(checked) => checked_items.push(checked),
                        Err(failure) => return Ok(Err(failure)),
                    }
                }
                Ok(Ok(Val::Vec(checked_items)))
            }
            "map" => {
                let Val::Map(entries) = value else {
                    return Ok(Err(self.base_mismatch("Map", &value, path)));
                };
                let elem = get(node, "elem").cloned().unwrap_or(Val::Null);
                let mut checked_entries = indexmap::IndexMap::with_capacity(entries.len());
                for (key, item) in entries.into_iter() {
                    path.push(key.clone());
                    let result = self.check_value(item, &elem, path)?;
                    path.pop();
                    match result {
                        Ok(checked) => {
                            checked_entries.insert(key, checked);
                        }
                        Err(failure) => return Ok(Err(failure)),
                    }
                }
                Ok(Ok(Val::Map(Box::new(checked_entries))))
            }
            "struct" => self.check_struct(value, node, path),
            "enum" => match get(node, "invariant") {
                Some(invariant) if !matches!(invariant, Val::Null) => {
                    let invariant = invariant.clone();
                    self.check_value(value, &invariant, path)
                }
                _ => Ok(Ok(value)),
            },
            "named" => {
                let name = get_str(node, "type").unwrap_or_default().to_string();
                match self.named_constraint_spec(&name) {
                    Some(spec) => self.check_value(value, &spec, path),
                    None => Ok(Ok(value)),
                }
            }
            "refine" => self.check_refinement(value, node, path),
            _ => Ok(Ok(value)),
        }
    }

    fn base_mismatch(&self, expected: &str, value: &Val, path: &[Val]) -> Box<Failure> {
        self.failure(
            FailureKind::BaseMismatch {
                expected: expected.to_string(),
            },
            value,
            path,
        )
    }

    /// A strict base-type check. The only conversions are exact numeric
    /// ones: an `Int` becomes a `Dec`, and an integral `Dec` an `Int`.
    fn check_base(&self, value: Val, expected: &str, path: &[Val]) -> Result<Val, Box<Failure>> {
        let matches = match (expected, &value) {
            ("Any", _) => true,
            ("Int", Val::Int(_))
            | ("Dec", Val::Dec(_))
            | ("Str", Val::Str(_))
            | ("Bool", Val::Bool(_))
            | ("Null", Val::Null)
            | ("Vec", Val::Vec(_))
            | ("Map", Val::Map(_))
            | ("Bytes", Val::Bytes(_))
            | ("Byte", Val::Byte(_))
            | ("Fn", Val::Box(_) | Val::Str(_)) => true,
            ("Int", Val::Dec(d)) => {
                if d.floor() == *d
                    && let Ok(n) = d.to_i64()
                {
                    return Ok(Val::Int(n));
                }
                false
            }
            ("Dec", Val::Int(n)) => return Ok(Val::Dec(D256::from(*n))),
            _ => false,
        };
        if matches {
            Ok(value)
        } else {
            Err(self.base_mismatch(expected, &value, path))
        }
    }

    fn check_struct(
        &mut self,
        value: Val,
        node: &Val,
        path: &mut Vec<Val>,
    ) -> VmResult<Result<Val, Box<Failure>>> {
        let type_name = get_str(node, "type").unwrap_or("Map").to_string();
        if !matches!(value, Val::Map(_)) {
            return Ok(Err(self.base_mismatch(
                short_name(&type_name),
                &value,
                path,
            )));
        }
        let mut current = value;
        if let Some(Val::Map(field_specs)) = get(node, "fields").cloned() {
            for (field, field_spec) in field_specs.iter() {
                let field_value = match crate::val::as_fields(&current) {
                    Val::Map(fields) => fields.get(field).cloned().unwrap_or(Val::Null),
                    _ => Val::Null,
                };
                path.push(field.clone());
                let result = self.check_value(field_value.clone(), field_spec, path)?;
                path.pop();
                match result {
                    Ok(checked) => {
                        if checked != field_value {
                            current = with_field(&current, field.clone(), checked);
                        }
                    }
                    Err(failure) => return Ok(Err(failure)),
                }
            }
        }
        match get(node, "invariant") {
            Some(invariant) if !matches!(invariant, Val::Null) => {
                let invariant = invariant.clone();
                self.check_value(current, &invariant, path)
            }
            _ => Ok(Ok(current)),
        }
    }

    fn check_refinement(
        &mut self,
        value: Val,
        node: &Val,
        path: &mut Vec<Val>,
    ) -> VmResult<Result<Val, Box<Failure>>> {
        let constraint = get_str(node, "constraint")
            .unwrap_or("<inline>")
            .to_string();
        let predicate_source = get_str(node, "source").unwrap_or_default().to_string();
        let location = get(node, "location").cloned().unwrap_or(Val::Null);
        let base = get(node, "base").cloned().unwrap_or(Val::Null);
        let checked = match self.check_value(value, &base, path)? {
            Ok(checked) => checked,
            Err(mut failure) => {
                if failure.constraint.is_empty() {
                    failure.constraint = constraint;
                    failure.predicate = predicate_source;
                    failure.location = location;
                }
                return Ok(Err(failure));
            }
        };
        // Canonical shapes are checked here without calling the predicate.
        if let Some(fast) = get(node, "fast")
            && let Some(passed) = fast_check(&checked, fast)
        {
            if passed {
                return Ok(Ok(checked));
            }
            let mut failure = self.failure(FailureKind::Violation, &checked, path);
            failure.constraint = constraint;
            failure.predicate = predicate_source;
            failure.location = location;
            return Ok(Err(failure));
        }
        let predicate = get_str(node, "pred").unwrap_or_default().to_string();
        let kind = match self.run_predicate(&predicate, &checked) {
            PredicateOutcome::Pass => return Ok(Ok(checked)),
            PredicateOutcome::Fail => FailureKind::Violation,
            PredicateOutcome::NotBool(type_name) => FailureKind::Evaluation {
                cause: Val::from(format!(
                    "the predicate returned {}, not a Bool",
                    with_article(&type_name)
                )),
            },
            PredicateOutcome::Err(cause) => FailureKind::Evaluation { cause },
            PredicateOutcome::Halted(message) => FailureKind::Evaluation {
                cause: Val::from(message),
            },
            PredicateOutcome::Native(name) => FailureKind::Evaluation {
                cause: Val::from(format!(
                    "the predicate calls `{name}`, which isn't allowed in a `where` predicate"
                )),
            },
            PredicateOutcome::Budget(budget, limit) => FailureKind::Budget { budget, limit },
        };
        let mut failure = self.failure(kind, &checked, path);
        failure.constraint = constraint;
        failure.predicate = predicate_source;
        failure.location = location;
        Ok(Err(failure))
    }

    /// Run a predicate function on a candidate, with the JIT off, natives
    /// restricted to pure ones, and a budget. A halt inside the predicate
    /// (`fail()`, a runtime error) is contained and reported as its outcome.
    fn run_predicate(&mut self, predicate: &str, candidate: &Val) -> PredicateOutcome {
        let outermost = self.constraint.is_none();
        if outermost {
            self.constraint = Some(ConstraintGuard::new(self.user_function_depth));
        }
        let failed_before = self.has_failed();
        let cancelled_before = self.has_cancelled();
        let result =
            crate::lang::hot::coll::call_function_with_vm(self, &Val::from(predicate), candidate);
        let stopped = self.constraint.as_ref().and_then(|g| g.stopped.clone());
        if outermost {
            self.constraint = None;
        }
        let halted =
            (!failed_before && self.has_failed()) || (!cancelled_before && self.has_cancelled());
        let halt_message = if halted {
            self.get_failure().map(|f| f.msg)
        } else {
            None
        };
        if halted {
            self.reset_failure_state();
            self.reset_cancellation_state();
        }
        if let Some(stop) = stopped {
            return match stop {
                GuardStop::Fuel => PredicateOutcome::Budget("instructions", PREDICATE_FUEL as i64),
                GuardStop::Depth => PredicateOutcome::Budget("depth", PREDICATE_DEPTH as i64),
                GuardStop::Native(name) => PredicateOutcome::Native(name),
            };
        }
        match result {
            Ok(Val::Bool(true)) => PredicateOutcome::Pass,
            Ok(Val::Bool(false)) => PredicateOutcome::Fail,
            Ok(value) if value.is_err() => {
                PredicateOutcome::Err(value.unwrap_err().cloned().unwrap_or(Val::Null))
            }
            Ok(value) => PredicateOutcome::NotBool(self.type_name_of(&value)),
            Err(message) => PredicateOutcome::Halted(halt_message.unwrap_or(message)),
        }
    }

    /// The `Result.Err` holding a `ConstraintError` for a failed check.
    fn constraint_error(&self, failure: &Failure, boundary: &Boundary) -> Val {
        let subject = boundary.subject();
        let at = if failure.path.is_empty() {
            String::new()
        } else {
            format!(" at `{}`", path_text(&failure.path))
        };
        let constraint = if failure.constraint.is_empty() {
            boundary
                .declared_type
                .clone()
                .unwrap_or_else(|| "<inline>".to_string())
        } else {
            failure.constraint.clone()
        };
        // A type's own constructor or update names the type already.
        let names_subject = matches!(boundary.kind.as_str(), "constructor" | "update")
            && boundary.declared_type.as_deref() == Some(constraint.as_str());
        let what = if constraint == "<inline>" || constraint.contains("$Where-") || names_subject {
            format!("`{}`", failure.predicate)
        } else if failure.predicate.is_empty() {
            format!("`{}`", short_name(&constraint))
        } else {
            format!("`{}` ({})", short_name(&constraint), failure.predicate)
        };
        let (variant, message, extra) = match &failure.kind {
            FailureKind::Violation => ("Violation", format!("{subject}{at} failed {what}"), vec![]),
            FailureKind::BaseMismatch { expected } => (
                "Violation",
                format!(
                    "{subject}{at} expected {expected}, got {}",
                    failure.value_type
                ),
                vec![],
            ),
            FailureKind::NotOneOf => (
                "Violation",
                format!("{subject}{at} must be one of {}", failure.predicate),
                vec![],
            ),
            FailureKind::Evaluation { cause } => {
                let cause_text = match cause {
                    Val::Str(s) => s.to_string(),
                    other => format!(
                        "the predicate returned an error ({})",
                        self.type_name_of(other)
                    ),
                };
                (
                    "EvaluationFailed",
                    format!("{subject}{at}: could not check {what}: {cause_text}"),
                    vec![("cause", cause.clone())],
                )
            }
            FailureKind::Budget { budget, limit } => (
                "BudgetExceeded",
                format!(
                    "{subject}{at}: {what} ran out of its {} budget",
                    if *budget == "depth" {
                        "call-depth"
                    } else {
                        "instruction"
                    }
                ),
                vec![("budget", Val::from(*budget)), ("limit", Val::Int(*limit))],
            ),
        };
        let optional = |value: &Option<String>| value.clone().map(Val::from).unwrap_or(Val::Null);
        let constraint_name =
            if constraint.starts_with("$Where-") || constraint.contains("/$Where-") {
                "<inline>".to_string()
            } else {
                constraint
            };
        let mut fields = vec![
            ("message", Val::from(message)),
            ("constraint", Val::from(constraint_name)),
            ("predicate", Val::from(failure.predicate.clone())),
            ("boundary", Val::from(boundary.kind.clone())),
            ("path", Val::Vec(failure.path.clone())),
            ("declared-type", optional(&boundary.declared_type)),
            ("function", optional(&boundary.function)),
            ("parameter", optional(&boundary.parameter)),
            ("location", failure.location.clone()),
            ("value-type", Val::from(failure.value_type.clone())),
        ];
        fields.extend(extra);
        let payload = Val::map_from_iter(fields.into_iter().map(|(k, v)| (Val::from(k), v)));
        Val::err(Val::map_from_iter([
            (
                Val::from("$type"),
                Val::from(format!("::hot::type/ConstraintError.{variant}")),
            ),
            (Val::from("$val"), payload),
        ]))
    }
}

/// Check a value against a predicate's fast form (see
/// `constraint_specs::fast_form`). `None` when the value doesn't fit the
/// shapes, so the predicate itself must decide.
fn fast_check(value: &Val, fast: &Val) -> Option<bool> {
    let Val::Map(keywords) = fast else {
        return None;
    };
    let number = |v: &Val| match v {
        Val::Int(n) => Some(D256::from(*n)),
        Val::Dec(d) => Some(*d),
        _ => None,
    };
    // Hot's `length`: bytes of a Str, entries of a Vec or Map.
    let length = |v: &Val| match crate::val::as_fields(v) {
        Val::Str(s) => Some(s.len() as i64),
        Val::Vec(items) => Some(items.len() as i64),
        Val::Map(entries) => Some(entries.len() as i64),
        _ => None,
    };
    let int = |v: &Val| match v {
        Val::Int(n) => Some(*n),
        _ => None,
    };
    let mut passed = true;
    for (keyword, bound) in keywords.iter() {
        let Val::Str(keyword) = keyword else {
            return None;
        };
        let ok = match keyword.as_ref() {
            "minimum" => number(value)? >= number(bound)?,
            "maximum" => number(value)? <= number(bound)?,
            "exclusiveMinimum" => number(value)? > number(bound)?,
            "exclusiveMaximum" => number(value)? < number(bound)?,
            "$minLength" => length(value)? >= int(bound)?,
            "$maxLength" => length(value)? <= int(bound)?,
            // Only same-typed values decide here; `eq` and `in` may treat
            // other pairs (such as `5` and `5.0`) as equal.
            "enum" => match bound {
                Val::Vec(values) if values.contains(value) => true,
                Val::Vec(values)
                    if values
                        .iter()
                        .all(|v| std::mem::discriminant(v) == std::mem::discriminant(value)) =>
                {
                    false
                }
                _ => return None,
            },
            "const" if std::mem::discriminant(value) == std::mem::discriminant(bound) => {
                value == bound
            }
            "const" => return None,
            // `::hot::regex/is-match` itself, with its pattern cache and
            // limits. Its errors, such as a value that isn't a Str, are the
            // predicate's to report.
            "pattern" => match crate::lang::hot::regex::is_match(&[value.clone(), bound.clone()]) {
                crate::lang::hot::r#type::HotResult::Ok(Val::Bool(matched)) => matched,
                _ => return None,
            },
            _ => return None,
        };
        passed &= ok;
    }
    Some(passed)
}

/// `an Int`, `a Str`.
fn with_article(type_name: &str) -> String {
    let article = if type_name.starts_with(['A', 'E', 'I', 'O', 'U']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {type_name}")
}

/// `value` with one field replaced, keeping a typed value's tag.
fn with_field(value: &Val, field: Val, field_value: Val) -> Val {
    let mut fields = match crate::val::as_fields(value) {
        Val::Map(fields) => fields.as_ref().clone(),
        _ => indexmap::IndexMap::new(),
    };
    fields.insert(field, field_value);
    crate::val::with_type_of(value, Val::Map(Box::new(fields)))
}

/// The value at `key` inside `value`, looking through a typed value's tag.
fn path_step(value: &Val, key: &Val) -> Option<Val> {
    match value {
        Val::Map(entries) => {
            let fields = match entries.get(&Val::from("$val")) {
                Some(payload) if crate::val::tagged_type_name(entries).is_some() => payload,
                _ => value,
            };
            match fields {
                Val::Map(fields) => fields.get(key).cloned(),
                Val::Vec(items) => vec_item(items, key),
                _ => None,
            }
        }
        Val::Vec(items) => vec_item(items, key),
        _ => None,
    }
}

fn vec_item(items: &[Val], key: &Val) -> Option<Val> {
    match key {
        Val::Int(index) => usize::try_from(*index)
            .ok()
            .and_then(|index| items.get(index).cloned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_patterns_use_regex_is_match() {
        let fast = |pattern: &str| Val::map_from_iter([(Val::from("pattern"), Val::from(pattern))]);
        assert_eq!(
            fast_check(&Val::from("AB-12"), &fast("^[A-Z]{2}-")),
            Some(true)
        );
        assert_eq!(
            fast_check(&Val::from("ab-12"), &fast("^[A-Z]{2}-")),
            Some(false)
        );
        // What `is-match` itself rejects goes to the predicate.
        assert_eq!(fast_check(&Val::Int(5), &fast("^[A-Z]")), None);
        assert_eq!(fast_check(&Val::from("A"), &fast("(")), None);
    }
}
