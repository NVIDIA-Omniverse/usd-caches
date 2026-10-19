// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Filter types for gating tracing layers by log level with optional per-module overrides.
//!
//! [`FilterGatedLayer`] wraps any tracing layer and gates event emission using a [`FilterSpec`].

use tracing::metadata::LevelFilter;

/// A single per-module filter entry.
struct ModuleFilter {
    /// The module name prefix (without a trailing `*`).
    prefix: String,
    /// Precomputed `"{prefix}::"` for cheap path-prefix matching.
    prefix_sep: String,
    /// When true, match any target that starts with `prefix` (bare string prefix).
    /// When false, match exact `prefix` or targets starting with `prefix::`.
    wildcard: bool,
    level: LevelFilter,
}

/// Parsed filter specification: a global level plus optional per-module overrides.
///
/// Built via [`FilterSpec::from_level_and_overrides`], whose `overrides` string has the format
/// `module[*]=level[,module2[*]=level2]...`
///
/// Examples of `overrides`:
/// ```text
/// ovdc=info
/// ovdc=info,ovdc::grpc=warn
/// ovdc*=info          -- matches ovdc, ovdc_trace, ovdc::anything
/// ```
///
/// Matching rules:
/// - The most specific (longest prefix) module entry wins.
/// - Without `*`: `module=level` also applies to all sub-modules (`module::*`).
/// - With `*`: bare string prefix match (`foo*` matches `foo_bar`, `foo::x`, …).
/// - Falls back to the global level when no module entry matches.
#[derive(Clone, Debug)]
pub struct FilterSpec {
    global: LevelFilter,
    /// Sorted by descending `prefix.len()` so the first match is always the most specific.
    modules: Vec<ModuleFilter>,
}

// Manual Clone because ModuleFilter doesn't derive it (it contains non-Copy fields).
impl Clone for ModuleFilter {
    fn clone(&self) -> Self {
        Self {
            prefix: self.prefix.clone(),
            prefix_sep: self.prefix_sep.clone(),
            wildcard: self.wildcard,
            level: self.level,
        }
    }
}

impl std::fmt::Debug for ModuleFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{}={:?}",
            self.prefix,
            if self.wildcard { "*" } else { "" },
            self.level
        )
    }
}

/// Parse `module[*]=level` entries (each item of `parts`) into sorted [`ModuleFilter`]s.
/// Entries that don't match `lhs=rhs` or whose level fails to parse are silently skipped.
fn parse_module_overrides<'a>(parts: impl Iterator<Item = &'a str>) -> Vec<ModuleFilter> {
    let mut modules: Vec<ModuleFilter> = parts
        .filter_map(|part| {
            let (lhs, rhs) = part.split_once('=')?;
            let level = rhs.trim().parse::<LevelFilter>().ok()?;
            let lhs = lhs.trim();
            let (prefix, wildcard) = if let Some(stripped) = lhs.strip_suffix('*') {
                (stripped, true)
            } else {
                (lhs, false)
            };
            let prefix_sep = format!("{}::", prefix);
            Some(ModuleFilter {
                prefix: prefix.to_string(),
                prefix_sep,
                wildcard,
                level,
            })
        })
        .collect();

    // Most-specific (longest prefix) first.
    modules.sort_by_key(|m| std::cmp::Reverse(m.prefix.len()));
    modules
}

impl FilterSpec {
    /// Build a [`FilterSpec`] from an explicit global `level` plus an `overrides` string
    /// containing only `module[*]=level` entries, e.g. `"ovdc::grpc=debug,ovdc::lru=trace"`.
    ///
    /// This lets a sink's base level and its per-module overrides be configured as two
    /// independent settings.
    pub fn from_level_and_overrides(level: LevelFilter, overrides: &str) -> Self {
        Self {
            global: level,
            modules: parse_module_overrides(overrides.split(',')),
        }
    }

    /// Return the effective [`LevelFilter`] for the given event `target`.
    pub fn level_for(&self, target: &str) -> LevelFilter {
        for m in &self.modules {
            let matched = if m.wildcard {
                target.starts_with(&*m.prefix)
            } else {
                target == m.prefix || target.starts_with(&*m.prefix_sep)
            };
            if matched {
                return m.level;
            }
        }
        self.global
    }
}

/// Wraps a tracing [`Layer`] and gates [`on_event`] using an atomically-swappable [`FilterSpec`].
///
/// Unlike `.with_filter()`, this does NOT create a `Filtered` wrapper, so it never touches the
/// `FilterState` thread-locals.  That avoids the `FilterState` counter mismatch that occurs when
/// `MetricsLayer` (which uses its own per-layer filter internally) is present in the same
/// subscriber tree.
///
/// Only [`on_event`] is gated by level. Span lifecycle callbacks (`on_new_span`, `on_record`,
/// `on_enter`, `on_exit`, `on_close`) are always forwarded to `inner` unconditionally, even for
/// spans below the configured level. This is required for correctness: formatters like
/// `tracing_subscriber::fmt`'s JSON layer lazily populate per-span state (e.g.
/// `FormattedFields`) in `on_new_span`, then unconditionally look it up for every ancestor span
/// when serializing any event that passes the filter. If a span's `on_new_span` were skipped
/// because the span itself was below the level threshold, but a nested event above the threshold
/// still fired, the formatter would panic looking up state that was never recorded for that
/// ancestor. Gating only `on_event` keeps span bookkeeping consistent across all layers while
/// still controlling which events are actually emitted.
pub(crate) struct FilterGatedLayer<L, S> {
    pub inner: L,
    pub filter: FilterSpec,
    pub _subscriber: std::marker::PhantomData<S>,
}

impl<L, S> tracing_subscriber::Layer<S> for FilterGatedLayer<L, S>
where
    L: tracing_subscriber::Layer<S> + Send + Sync + 'static,
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        let allowed = *meta.level() <= self.filter.level_for(meta.target());
        if allowed {
            self.inner.on_event(event, ctx);
        }
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.inner.on_new_span(attrs, id, ctx);
    }

    fn on_record(
        &self,
        span: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.inner.on_record(span, values, ctx);
    }

    fn on_enter(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        self.inner.on_enter(id, ctx);
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        self.inner.on_exit(id, ctx);
    }

    fn on_close(&self, id: tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        self.inner.on_close(id, ctx);
    }
}

/// Read the initial log level from `RUST_LOG`, falling back to `INFO`.
pub(crate) fn parse_rust_log_level() -> LevelFilter {
    std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.split(',').next()?.trim().parse().ok())
        .unwrap_or(LevelFilter::INFO)
}

/// Parse a bare level string (e.g. `"trace"`), logging a warning and returning `None` if it
/// doesn't parse instead of silently falling back to a default.
pub(crate) fn parse_level_or_warn(setting_name: &str, s: &str) -> Option<LevelFilter> {
    match s.parse::<LevelFilter>() {
        Ok(level) => Some(level),
        Err(_) => {
            tracing::warn!(
                setting = setting_name,
                value = s,
                "failed to parse log level; falling back to default"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_only() {
        let s = FilterSpec::from_level_and_overrides(LevelFilter::WARN, "");
        assert_eq!(s.level_for("anything"), LevelFilter::WARN);
    }

    #[test]
    fn module_override() {
        let s = FilterSpec::from_level_and_overrides(LevelFilter::ERROR, "ovdc=info");
        assert_eq!(s.level_for("other"), LevelFilter::ERROR);
        assert_eq!(s.level_for("ovdc"), LevelFilter::INFO);
        assert_eq!(s.level_for("ovdc::grpc"), LevelFilter::INFO);
    }

    #[test]
    fn module_specificity() {
        let s =
            FilterSpec::from_level_and_overrides(LevelFilter::ERROR, "ovdc=info,ovdc::grpc=warn");
        assert_eq!(s.level_for("ovdc"), LevelFilter::INFO);
        assert_eq!(s.level_for("ovdc::grpc"), LevelFilter::WARN);
        assert_eq!(s.level_for("ovdc::grpc::server"), LevelFilter::WARN);
        assert_eq!(s.level_for("ovdc::lru"), LevelFilter::INFO);
    }

    #[test]
    fn wildcard_prefix() {
        let s = FilterSpec::from_level_and_overrides(LevelFilter::ERROR, "ovdc*=debug");
        assert_eq!(s.level_for("ovdc"), LevelFilter::DEBUG);
        assert_eq!(s.level_for("ovdc_trace"), LevelFilter::DEBUG);
        assert_eq!(s.level_for("ovdc::foo"), LevelFilter::DEBUG);
        assert_eq!(s.level_for("other"), LevelFilter::ERROR);
    }

    #[test]
    fn no_partial_path_match() {
        // "ovdc" without wildcard must not match "ovdc_trace"
        let s = FilterSpec::from_level_and_overrides(LevelFilter::ERROR, "ovdc=info");
        assert_eq!(s.level_for("ovdc_trace"), LevelFilter::ERROR);
    }

    #[test]
    fn from_level_and_overrides_sets_global_and_modules_independently() {
        let s = FilterSpec::from_level_and_overrides(LevelFilter::TRACE, "ovdc::grpc=warn");
        assert_eq!(s.level_for("anything"), LevelFilter::TRACE);
        assert_eq!(s.level_for("ovdc::grpc"), LevelFilter::WARN);
        assert_eq!(s.level_for("ovdc::grpc::server"), LevelFilter::WARN);
    }

    #[test]
    fn from_level_and_overrides_empty_overrides_is_global_only() {
        let s = FilterSpec::from_level_and_overrides(LevelFilter::DEBUG, "");
        assert_eq!(s.level_for("anything"), LevelFilter::DEBUG);
    }

    /// Regression test for a panic where the JSON `fmt` layer's `FormattedFields` lookup would
    /// `.expect()`-panic when serializing an event nested inside a span whose own level was
    /// below the configured filter (e.g. a `trace_span!` filtered out while an `info!` event
    /// inside it still passed the filter). See `FilterGatedLayer` doc comment for the mechanism.
    #[test]
    fn json_layer_does_not_panic_on_event_inside_filtered_out_span() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry::Registry;

        let json_layer = FilterGatedLayer::<_, Registry> {
            inner: tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::sink),
            filter: FilterSpec::from_level_and_overrides(LevelFilter::INFO, ""),
            _subscriber: std::marker::PhantomData,
        };
        let subscriber = Registry::default().with(json_layer);

        tracing::subscriber::with_default(subscriber, || {
            // This span is below the INFO filter threshold, so it must not receive a
            // `FormattedFields` entry via `on_new_span` under the old (buggy) gating.
            let span = tracing::trace_span!("filtered_out_span");
            let _guard = span.enter();

            // This event passes the INFO filter and is nested inside the filtered-out span.
            // Under the old behavior, the JSON formatter would walk the span chain (including
            // "filtered_out_span") and panic looking up its missing `FormattedFields`.
            tracing::info!("this must not panic the logger");
        });
    }

    /// Same scenario as above, but with several levels of nesting and mixed allowed/filtered
    /// spans, to make sure the fix isn't just papering over the single-span case.
    #[test]
    fn json_layer_does_not_panic_with_nested_mixed_level_spans() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry::Registry;

        let json_layer = FilterGatedLayer::<_, Registry> {
            inner: tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::sink),
            filter: FilterSpec::from_level_and_overrides(LevelFilter::INFO, ""),
            _subscriber: std::marker::PhantomData,
        };
        let subscriber = Registry::default().with(json_layer);

        tracing::subscriber::with_default(subscriber, || {
            let outer = tracing::info_span!("allowed_outer_span");
            let _outer_guard = outer.enter();

            let inner = tracing::debug_span!("filtered_inner_span");
            let _inner_guard = inner.enter();

            let innermost = tracing::trace_span!("filtered_innermost_span");
            let _innermost_guard = innermost.enter();

            tracing::warn!("event nested three levels deep must not panic");
        });
    }
}
