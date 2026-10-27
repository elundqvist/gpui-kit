//! The queries a language is highlighted by, compiled once for the process.
//!
//! Compiling a language's query sources takes from 4 ms (Python) to 12
//! (Rust), 22 (Kotlin) and 51 (Elixir); a highlighter's own, which holds
//! the injections' and the locals' too, 13 for Rust and 39 for
//! TypeScript. A highlighter compiled its own on the drawing thread when
//! it was made, in the frame after a file opened (the editor's has it
//! compiled on a thread now, elundqvist/kvist#597), and every injected
//! language's again in each highlighter that met it: the open of a
//! Markdown file with a Rust, a JSON and a Python fence took 18 ms of the
//! drawing thread's `editor parse`, and so did the same file opened
//! again, and the first key typed in a new Rust fence 12 to 15
//! (elundqvist/kvist#593).
//!
//! A language's queries are kept for as long as the process lives, one set
//! for all its highlighters, and only for the languages some text has
//! named: not every registered grammar's. A language registered again
//! under its name with other sources is compiled again, and the sources
//! it had stay, for a few: Kvist registers two configs under the name of
//! each language with injections, the file's own with them and the span a
//! fence names without, and a fence in a README replaced the file's
//! queries with the span's, so the next file of the language was the first
//! again (elundqvist/kvist#597).

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, LazyLock, Mutex, OnceLock, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    thread::ThreadId,
};

use anyhow::{Context as _, Result, anyhow};
use gpui::{BackgroundExecutor, SharedString};
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
    /// The tasks sent to compile `host` (`compiling_host`), each under the
    /// thread that sent it, with the futures of that thread waiting for it.
    compiling: Mutex<HashMap<ThreadId, Vec<Waker>>>,
    /// How many tasks were sent.
    #[cfg(test)]
    sent: std::sync::atomic::AtomicUsize,
}

/// How many configs registered under one name keep their queries: the
/// oldest goes where a fifth is met.
const PER_NAME: usize = 4;

static CACHE: LazyLock<Mutex<HashMap<SharedString, Vec<Arc<Entry>>>>> =
    LazyLock::new(Default::default);

fn entry(config: &LanguageConfig) -> Arc<Entry> {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let entries = cache.entry(config.name.clone()).or_default();
    if let Some(entry) = entries.iter().find(|entry| entry.config == *config) {
        return entry.clone();
    }
    if entries.len() == PER_NAME {
        entries.remove(0);
    }
    let entry = Arc::new(Entry {
        config: config.clone(),
        host: OnceLock::new(),
        injected: OnceLock::new(),
        warming: AtomicBool::new(false),
        compiling: Mutex::default(),
        #[cfg(test)]
        sent: Default::default(),
    });
    entries.push(entry.clone());
    entry
}

/// The queries a highlighter of `config`'s language takes, compiled now
/// where no one has yet, or waited for where another thread is at it.
pub(super) fn host(config: &LanguageConfig) -> Result<Arc<HostQueries>> {
    host_of(&entry(config), config)
}

fn host_of(entry: &Entry, config: &LanguageConfig) -> Result<Arc<HostQueries>> {
    entry
        .host
        .get_or_init(|| {
            compile_host(config)
                .map(Arc::new)
                .map_err(|error| format!("{error:#}"))
        })
        .clone()
        .map_err(|error| anyhow!(error))
}

/// Whether `host` has the queries of `config`'s language, or knows they do
/// not compile: it returns at once then, and compiles nothing.
pub(super) fn host_compiled(config: &LanguageConfig) -> bool {
    entry(config).host.get().is_some()
}

/// Send a task of `executor` to compile `config`'s host queries, where no
/// one has them and this thread has none at it, and give a future that is
/// ready once they are, so that a second highlighter of the language, made
/// while the first compiles, waits for that task rather than hold a thread
/// of its own in `host` until it ends. The task runs whatever becomes of
/// the future. Ready at once where the queries are compiled, and also where
/// the task is gone without them, which `host` then compiles in place.
///
/// A task is shared by the highlighters of the thread that sent it, which
/// is the drawing thread's, and its end wakes only that thread's futures:
/// the executors of two threads, as those of two tests running side by
/// side are, do not wake each other's tasks.
pub(super) fn compiling_host(config: &LanguageConfig, executor: &BackgroundExecutor) -> HostCompiled {
    let entry = entry(config);
    let owner = std::thread::current().id();
    if let Some(task) = claim(&entry, owner) {
        let config = config.clone();
        executor
            .spawn(async move {
                let task = task;
                let _ = host_of(&task.entry, &config);
            })
            .detach();
    }
    HostCompiled { entry, owner }
}

/// The task `owner` is to send to compile `entry`'s host queries, where no
/// one has them and it has none at it.
fn claim(entry: &Arc<Entry>, owner: ThreadId) -> Option<Compiled> {
    let mut compiling = entry.compiling.lock().unwrap_or_else(PoisonError::into_inner);
    if entry.host.get().is_some() || compiling.contains_key(&owner) {
        return None;
    }
    compiling.insert(owner, Vec::new());
    #[cfg(test)]
    entry.sent.fetch_add(1, Ordering::Relaxed);
    Some(Compiled { entry: entry.clone(), owner })
}

/// How many tasks `compiling_host` sent for `config`'s host queries.
#[cfg(test)]
pub(super) fn host_tasks_sent(config: &LanguageConfig) -> usize {
    entry(config).sent.load(Ordering::Relaxed)
}

/// The task that compiles an entry's `host`: where it ends, with the
/// queries or not, or is dropped before it ran, the futures waiting for it
/// go on.
struct Compiled {
    entry: Arc<Entry>,
    owner: ThreadId,
}

impl Drop for Compiled {
    fn drop(&mut self) {
        let waiting = {
            let mut compiling = self.entry.compiling.lock().unwrap_or_else(PoisonError::into_inner);
            compiling.remove(&self.owner).unwrap_or_default()
        };
        for waker in waiting {
            waker.wake();
        }
    }
}

/// Ready once the entry's host queries are compiled (`compiling_host`).
pub(super) struct HostCompiled {
    entry: Arc<Entry>,
    owner: ThreadId,
}

impl Future for HostCompiled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut compiling = self.entry.compiling.lock().unwrap_or_else(PoisonError::into_inner);
        if self.entry.host.get().is_some() {
            return Poll::Ready(());
        }
        let Some(waiting) = compiling.get_mut(&self.owner) else {
            return Poll::Ready(());
        };
        if !waiting.iter().any(|waker| waker.will_wake(cx.waker())) {
            waiting.push(cx.waker().clone());
        }
        Poll::Pending
    }
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

#[cfg(all(test, feature = "tree-sitter-languages"))]
mod tests {
    use super::super::LanguageRegistry;
    use super::*;

    /// Rust's grammar and highlights under `name`, naming `others` as the
    /// languages it injects: configs of one name that differ.
    fn config(name: &str, others: &[&str]) -> LanguageConfig {
        let rust = LanguageRegistry::singleton().language("rust").unwrap();
        LanguageConfig::new(
            name,
            rust.language.clone().unwrap(),
            others.iter().map(|other| SharedString::from(other.to_string())).collect(),
            &rust.highlights,
            "",
            "",
        )
    }

    /// Configs registered under one name, as Kvist registers a language
    /// with injections, the file's own and the span a fence names, keep
    /// their queries side by side: the one met last replaced the other's
    /// in the cache, so a fence in a README made the next file of its
    /// language the first again, and the oldest of more than a few goes
    /// (elundqvist/kvist#597).
    #[test]
    fn test_configs_of_one_name_each_keep_their_queries() {
        let name = "kvist-test-queries-one-name";
        let (file, span) = (config(name, &["markdown"]), config(name, &[]));
        let file_queries = host(&file).unwrap();
        let span_queries = host(&span).unwrap();
        assert!(!Arc::ptr_eq(&file_queries.query, &span_queries.query));
        // a fence uses the span's highlights, and neither is compiled again
        assert!(injected(&span).is_some());
        assert!(host_compiled(&file) && host_compiled(&span));
        assert!(Arc::ptr_eq(&host(&file).unwrap().query, &file_queries.query));
        assert!(Arc::ptr_eq(&host(&span).unwrap().query, &span_queries.query));

        let name = "kvist-test-queries-many-of-one-name";
        let configs: Vec<_> = (0..=PER_NAME).map(|i| config(name, &[&format!("l{i}")])).collect();
        for config in &configs {
            host(config).unwrap();
        }
        assert!(configs[1..].iter().all(host_compiled), "the newest {PER_NAME} stay");
        assert!(!host_compiled(&configs[0]), "the oldest went");
    }

    /// A future waiting for the task that compiles the queries goes on
    /// when the task ends, with the queries or without them.
    #[test]
    fn test_a_future_waiting_for_the_queries_goes_on_when_the_task_ends() {
        use std::task::Wake;

        struct Woken(AtomicBool);
        impl Wake for Woken {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let owner = std::thread::current().id();
        for compiled in [true, false] {
            let config = config(&format!("kvist-test-queries-waiting-{compiled}"), &[]);
            let entry = entry(&config);
            let task = claim(&entry, owner).expect("a task for the first");
            let mut waiting = HostCompiled { entry: entry.clone(), owner };
            let woken = Arc::new(Woken(AtomicBool::new(false)));
            let waker = Waker::from(woken.clone());
            let mut cx = Context::from_waker(&waker);
            assert_eq!(Pin::new(&mut waiting).poll(&mut cx), Poll::Pending);
            assert_eq!(Pin::new(&mut waiting).poll(&mut cx), Poll::Pending);
            assert_eq!(entry.compiling.lock().unwrap()[&owner].len(), 1, "once");
            assert!(!woken.0.load(Ordering::SeqCst));
            if compiled {
                host_of(&entry, &config).unwrap();
            }
            drop(task);
            assert!(woken.0.load(Ordering::SeqCst), "woken");
            assert_eq!(Pin::new(&mut waiting).poll(&mut cx), Poll::Ready(()));
            assert_eq!(host_compiled(&config), compiled);
        }
    }

    /// One task serves the thread that sent it, and none serves another
    /// thread, whose executor would have to wake the first's futures, or
    /// anyone once the queries are compiled.
    #[test]
    fn test_a_task_is_sent_once_for_a_thread_and_not_after_the_queries_are_compiled() {
        let config = config("kvist-test-queries-claimed", &[]);
        let entry = entry(&config);
        let (here, there) = (std::thread::current().id(), std::thread::spawn(|| std::thread::current().id()).join().unwrap());
        let task = claim(&entry, here).expect("the first");
        assert!(claim(&entry, here).is_none(), "one for the thread");
        let other = claim(&entry, there).expect("another thread has a task of its own");
        assert!(claim(&entry, there).is_none());
        assert_eq!(entry.sent.load(Ordering::Relaxed), 2);
        drop(task);
        let again = claim(&entry, here).expect("the first ended without queries");
        host_of(&entry, &config).unwrap();
        drop((again, other));
        assert!(claim(&entry, here).is_none() && claim(&entry, there).is_none(), "compiled");
    }
}
