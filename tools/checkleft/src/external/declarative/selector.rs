//! A jaq-backed JSON selector: evaluates any jq/jaq filter against a JSON
//! value, returning the matching items as the set of "rows" each finding is
//! projected from.
//!
//! The filter string is validated at parse time (syntax errors are caught
//! early) and compiled at evaluation time. Both of buildifier's filters work
//! without modification:
//!
//! - `.files[] | select(.formatted == false)` (format pass)
//! - `.files[].warnings[]` (lint pass)
//!
//! Richer jq expressions — variable binding, arithmetic, `|=`, arbitrary
//! function calls — are supported by jaq and do not need a separate wasm tier
//! unless they require side-effectful computation. That seam now lives at the
//! boundary of jaq's own feature set rather than at hand-rolled parsing limits.
//!
//! ## jaq prelude
//!
//! jaq 1.x parses `false`, `true`, and `null` as zero-arity filter calls
//! rather than literals (the token grammar does not have dedicated keyword
//! variants for them). `jaq_core::core()` does not define these filters
//! either. The prelude below registers the minimum set needed to support the
//! kinds of filters declarative checks are expected to use.

use std::sync::Arc;

use anyhow::{Result, bail};
use jaq_interpret::{Ctx, Filter, FilterT as _, Native, ParseCtx, RcIter, Val, ValR, ValRs};
use serde_json::Value;

/// A compiled jaq filter together with its source string.
///
/// Equality and ordering are based solely on the source string so that two
/// `Selector` values built from the same filter compare equal regardless of
/// when they were compiled.
pub struct Selector {
    filter: String,
    /// Compiled filter, shared cheaply across clones via Arc.
    compiled: Arc<Filter>,
}

impl std::fmt::Debug for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Selector").field("filter", &self.filter).finish()
    }
}

impl Clone for Selector {
    fn clone(&self) -> Self {
        Self {
            filter: self.filter.clone(),
            compiled: Arc::clone(&self.compiled),
        }
    }
}

impl PartialEq for Selector {
    fn eq(&self, other: &Self) -> bool {
        self.filter == other.filter
    }
}

impl Eq for Selector {}

impl Selector {
    /// Parse, compile, and cache a jaq filter string. Returns an error on any
    /// jq syntax or compile problem so bad manifests are rejected at load time,
    /// not at the moment a tool runs and produces output.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim().to_owned();
        if raw.is_empty() {
            bail!("selector filter must not be empty");
        }

        // jaq 1.x parses `false`, `true`, `null` as Call("false"/"true"/"null",
        // []) — not as literals. They are not provided by jaq_core::core().
        // Register them as natives, plus `empty` (also not in core) so that
        // `select(f)` from the stdlib can be defined.
        //
        // Order matters: natives must be registered before insert_defs so that
        // the stdlib def that uses `empty` compiles correctly.
        let (stdlib_defs, errs) = jaq_parse::parse(
            "def select(f): if f then . else empty end; \
             def not: if . then false else true end;",
            jaq_parse::defs(),
        );
        if !errs.is_empty() {
            bail!("jaq prelude parse errors: {errs:?}");
        }

        let (f, errs) = jaq_parse::parse(&raw, jaq_parse::main());
        if !errs.is_empty() {
            bail!("selector `{raw}` has jaq parse errors: {errs:?}");
        }
        let Some(f) = f else {
            bail!("selector `{raw}` produced no filter");
        };

        let mut ctx = ParseCtx::new(Vec::new());
        ctx.insert_natives(jaq_core::core());
        ctx.insert_native("empty".to_string(), 0, Native::new(jaq_empty));
        ctx.insert_native("false".to_string(), 0, Native::new(jaq_false));
        ctx.insert_native("true".to_string(), 0, Native::new(jaq_true));
        ctx.insert_native("null".to_string(), 0, Native::new(jaq_null));
        ctx.insert_defs(stdlib_defs.unwrap_or_default());
        let compiled = ctx.compile(f);
        if !ctx.errs.is_empty() {
            bail!("selector `{raw}` jaq compile errors: {} error(s)", ctx.errs.len());
        }

        Ok(Self {
            filter: raw,
            compiled: Arc::new(compiled),
        })
    }

    /// Evaluate the compiled filter against `root`, returning each output
    /// value as a separate row. Evaluation errors (e.g. type mismatches inside
    /// the filter) are surfaced as `Err`.
    pub fn select(&self, root: &Value) -> Result<Vec<Value>> {
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);
        let input = Val::from(root.clone());

        let mut rows = Vec::new();
        for result in self.compiled.as_ref().run((ctx, input)) {
            match result {
                Ok(val) => rows.push(Value::from(val)),
                Err(e) => bail!("selector `{}` evaluation error: {e}", self.filter),
            }
        }
        Ok(rows)
    }
}

fn jaq_empty<'a>(_: jaq_interpret::Args<'a>, _: (Ctx<'a>, Val)) -> ValRs<'a> {
    Box::new(core::iter::empty::<ValR>())
}

fn jaq_false<'a>(_: jaq_interpret::Args<'a>, _: (Ctx<'a>, Val)) -> ValRs<'a> {
    Box::new(core::iter::once(Ok(Val::Bool(false))))
}

fn jaq_true<'a>(_: jaq_interpret::Args<'a>, _: (Ctx<'a>, Val)) -> ValRs<'a> {
    Box::new(core::iter::once(Ok(Val::Bool(true))))
}

fn jaq_null<'a>(_: jaq_interpret::Args<'a>, _: (Ctx<'a>, Val)) -> ValRs<'a> {
    Box::new(core::iter::once(Ok(Val::Null)))
}
