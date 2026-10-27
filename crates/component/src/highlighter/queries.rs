//! The queries a language is highlighted by, compiled once for the process.
//!
//! Compiling a language's query sources takes from 4 ms (Python) to 12
//! (Rust), 22 (Kotlin) and 51 (Elixir); a highlighter's own, which holds
//! the injections' and the locals' too, 13 for Rust and 39 for
//! TypeScript. A highlighter compiled its own on the drawing thread when
//! it was made, in the frame after a file opened, and every injected
//! language's again in each highlighter that met it: the open of a
//! Markdown file with a Rust, a JSON and a Python fence took 18 ms of the
//! drawing thread's `editor parse`, and so did the same file opened
//! again, and the first key typed in a new Rust fence 12 to 15
//! (elundqvist/kvist#593).
//!
//! A language's queries are kept for as long as the process lives, one set
//! for all its highlighters, and only for the languages some text has
//! named: not every registered grammar's. A language registered again
//! under its name with other sources is compiled again.

use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock, Mutex, OnceLock, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, Result, anyhow};
use gpui::SharedString;
use tree_sitter::Query;

use super::{LanguageConfig, highlighter::pattern_root_kinds};

/// What a highlighter of a language has of its query sources: shared by
/// all of them, none of it changed once it is made.
pub(super) struct HostQueries {
    /// The injections', locals' and highlights' sources as one query, the
    /// patterns of the first two disabled: injection layers are computed
    /// apart from it.
    pub(super) query: Arc<Query>,
    /// The injections' alone, which the layers are computed by.
    pub(super) injections_query: Option<Arc<Query>>,
    /// The kind of node each pattern of the injections query starts at
    /// (`pattern_root_kinds`).
    pub(super) injection_roots: Arc<[Option<SharedString>]>,
    pub(super) locals_pattern_index: usize,
    pub(super) highlights_pattern_index: usize,
    pub(super) non_local_variable_patterns: Arc<[bool]>,
    pub(super) injection_content_capture_index: Option<u32>,
    pub(super) injection_language_capture_index: Option<u32>,
    pub(super) local_scope_capture_index: Option<u32>,
    pub(super) local_def_capture_index: Option<u32>,
    pub(super) local_def_value_capture_index: Option<u32>,
    pub(super) local_ref_capture_index: Option<u32>,
}

/// A language's queries, compiled where they were first asked for.
struct Entry {
    /// What the queries were compiled from: a language registered again
    /// under the name with other sources is another entry.
    config: LanguageConfig,
    host: OnceLock<std::result::Result<Arc<HostQueries>, String>>,
    /// The highlights alone, which a span of the language injected into
    /// another text is coloured by; `None` where they do not compile.
    injected: OnceLock<Option<Arc<Query>>>,
    /// A thread has been sent to compile `injected`.
    warming: AtomicBool,
}

static CACHE: LazyLock<Mutex<HashMap<SharedString, Arc<Entry>>>> = LazyLock::new(Default::default);

fn entry(config: &LanguageConfig) -> Arc<Entry> {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    match cache.get(&config.name) {
        Some(entry) if entry.config == *config => entry.clone(),
        _ => {
            let entry = Arc::new(Entry {
                config: config.clone(),
                host: OnceLock::new(),
                injected: OnceLock::new(),
                warming: AtomicBool::new(false),
            });
            cache.insert(config.name.clone(), entry.clone());
            entry
        }
    }
}

/// The queries a highlighter of `config`'s language takes, compiled now
/// where no one has yet, or waited for where another thread is at it.
pub(super) fn host(config: &LanguageConfig) -> Result<Arc<HostQueries>> {
    entry(config)
        .host
        .get_or_init(|| {
            compile_host(config)
                .map(Arc::new)
                .map_err(|error| format!("{error:#}"))
        })
        .clone()
        .map_err(|error| anyhow!(error))
}

/// `config`'s highlights as the query of a span of it injected into another
/// text, compiled now where no one has yet, or waited for where another
/// thread is at it. `None` where they do not compile, said once.
pub(super) fn injected(config: &LanguageConfig) -> Option<Arc<Query>> {
    let entry = entry(config);
    entry
        .injected
        .get_or_init(|| compile_injected(config))
        .clone()
}

/// What `injected` gives where it has been compiled, and `None` where it
/// has not, which this does not wait for or do.
pub(super) fn injected_if_compiled(config: &LanguageConfig) -> Option<Option<Arc<Query>>> {
    entry(config).injected.get().cloned()
}

/// Send a thread to compile the injected queries of `configs` that no one
/// has and none is on its way to: the drawing thread asks for them where it
/// leaves a span to the parse after the pause, which then finds them
/// compiled, or waits for the one still at it, and the parse for the span
/// after the next key finds them at once.
pub(super) fn compile_in_background(configs: Vec<LanguageConfig>) {
    let cold: Vec<_> = configs
        .into_iter()
        .map(|config| (entry(&config), config))
        .filter(|(entry, _)| {
            entry.injected.get().is_none() && !entry.warming.swap(true, Ordering::AcqRel)
        })
        .collect();
    if cold.is_empty() {
        return;
    }
    let warming: Vec<_> = cold.iter().map(|(entry, _)| entry.clone()).collect();
    let spawned = std::thread::Builder::new()
        .name("highlight-queries".into())
        .spawn(move || {
            for (entry, config) in cold {
                entry.injected.get_or_init(|| compile_injected(&config));
            }
        });
    if let Err(error) = spawned {
        tracing::warn!("no thread to compile highlight queries: {error}");
        for entry in warming {
            entry.warming.store(false, Ordering::Release);
        }
    }
}

fn compile_injected(config: &LanguageConfig) -> Option<Arc<Query>> {
    match Query::new(config.language.as_ref()?, &config.highlights) {
        Ok(query) => Some(Arc::new(query)),
        Err(error) => {
            tracing::error!(
                "failed to build injection query for {:?}: {:?}",
                config.name,
                error
            );
            None
        }
    }
}

/// The queries of a highlighter of `config`'s language, which has a
/// grammar.
///
/// https://github.com/tree-sitter/tree-sitter/blob/v0.26.8/crates/highlight/src/highlight.rs#L339
fn compile_host(config: &LanguageConfig) -> Result<HostQueries> {
    let grammar = config.language.as_ref().context("no grammar")?;

    // Concatenate the query strings, keeping track of the start offset of each section.
    let mut query_source = String::new();
    query_source.push_str(&config.injections);
    let locals_query_offset = query_source.len();
    query_source.push_str(&config.locals);
    let highlights_query_offset = query_source.len();
    query_source.push_str(&config.highlights);

    // Construct a single query by concatenating the three query strings, but record the
    // range of pattern indices that belong to each individual string.
    let mut query = Query::new(grammar, &query_source).context("new query")?;

    let mut locals_pattern_index = 0;
    let mut highlights_pattern_index = 0;
    for i in 0..(query.pattern_count()) {
        let pattern_offset = query.start_byte_for_pattern(i);
        if pattern_offset < highlights_query_offset {
            if pattern_offset < highlights_query_offset {
                highlights_pattern_index += 1;
            }
            if pattern_offset < locals_query_offset {
                locals_pattern_index += 1;
            }
        }
    }

    let injections_query = if !config.injections.is_empty() {
        Query::new(grammar, &config.injections).ok().map(Arc::new)
    } else {
        None
    };
    let injection_roots = injections_query
        .as_ref()
        .map_or_else(Vec::new, |q| pattern_root_kinds(q, &config.injections))
        .into();

    // Injection layers are computed separately during parsing, so do not
    // emit injection captures from the main highlight query.
    for pattern_index in 0..locals_pattern_index {
        query.disable_pattern(pattern_index);
    }

    // Find all of the highlighting patterns that are disabled for nodes that
    // have been identified as local variables.
    let non_local_variable_patterns = (0..query.pattern_count())
        .map(|i| {
            query
                .property_predicates(i)
                .iter()
                .any(|(prop, positive)| !*positive && prop.key.as_ref() == "local")
        })
        .collect();

    // Store the numeric ids for all of the special captures.
    let capture_in = |query: &Query, name: &str| {
        query
            .capture_names()
            .iter()
            .position(|capture| *capture == name)
            .map(|i| i as u32)
    };
    let injection_content_capture_index = injections_query
        .as_ref()
        .and_then(|q| capture_in(q, "injection.content"));
    let injection_language_capture_index = injections_query
        .as_ref()
        .and_then(|q| capture_in(q, "injection.language"));
    Ok(HostQueries {
        injections_query,
        injection_roots,
        locals_pattern_index,
        highlights_pattern_index,
        non_local_variable_patterns,
        injection_content_capture_index,
        injection_language_capture_index,
        local_scope_capture_index: capture_in(&query, "local.scope"),
        local_def_capture_index: capture_in(&query, "local.definition"),
        local_def_value_capture_index: capture_in(&query, "local.definition-value"),
        local_ref_capture_index: capture_in(&query, "local.reference"),
        query: Arc::new(query),
    })
}
