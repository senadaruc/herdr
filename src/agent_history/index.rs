use super::jump::{
    assistant_context_line, exact_prompt, validate_prompt, ExactHumanPrompt, JumpSourceGuard,
};
use super::{binding::HistoryBinding, claude, model::*, HistoryError};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::SystemTime;

const MAX_LINE: usize = 16 * 1024 * 1024;
const BODY_CACHE: usize = 16 * 1024 * 1024;
const JOURNAL: usize = 1000;
const MAX_SOURCES: usize = 4;
#[derive(Clone)]
struct Entry {
    offset: u64,
    first_offset: u64,
    compact: bool,
    len: usize,
    id: String,
    parent: Option<String>,
    revision: String,
    human: bool,
    prompt_fingerprint: Option<String>,
    preview: String,
    timestamp: Option<String>,
    order: usize,
    turn_id: String,
}
#[derive(Clone)]
struct Change {
    sequence: u64,
    id: String,
    removed: bool,
}
#[derive(Default)]
struct Source {
    jump_revision: Arc<AtomicU64>,
    session_id: String,
    path: PathBuf,
    identity: Option<String>,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    length: u64,
    complete_offset: u64,
    prefix_hash: String,
    entries: HashMap<String, Entry>,
    parent_links: HashMap<String, Option<String>>,
    order: Vec<String>,
    turns: Vec<ConversationTurn>,
    epoch: u64,
    revision: u64,
    sequence: u64,
    journal: VecDeque<Change>,
    cache: VecDeque<(String, ConversationMessage, usize)>,
    cache_bytes: usize,
    unsupported: bool,
    partial: bool,
}
#[derive(Default)]
pub struct ConversationReader {
    sources: HashMap<String, Source>,
    lru: VecDeque<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Cursor {
    generation: String,
    epoch: u64,
    kind: String,
    position: usize,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    block: Option<String>,
    #[serde(default)]
    slice: usize,
    #[serde(default)]
    query: Option<String>,
}
impl ConversationReader {
    pub fn read(
        &mut self,
        binding: &HistoryBinding,
        window: Window,
        limits: Limits,
    ) -> Result<ConversationPage, HistoryError> {
        let limits = limits.validate(matches!(window, Window::Turns { .. }))?;
        let source = match self.refresh(binding) {
            Ok(source) => source,
            Err(error)
                if matches!(
                    error.code.as_str(),
                    "source_missing"
                        | "access_denied"
                        | "unsupported_schema"
                        | "source_unavailable"
                ) =>
            {
                return Ok(ConversationPage::unavailable(
                    Some(binding.public()),
                    &error.code,
                ))
            }
            Err(error) => return Err(error),
        };
        if source.entries.is_empty() {
            return Ok(ConversationPage::unavailable(
                Some(binding.public()),
                if source.unsupported {
                    "unsupported_schema"
                } else {
                    "not_recorded"
                },
            ));
        }
        let mut page = ConversationPage::unavailable(Some(binding.public()), "ready");
        page.revision = source.revision.to_string();
        page.completeness = if source.unsupported {
            "partial_unsupported_records"
        } else if source.partial {
            "partial_trailing_record"
        } else {
            "source_available"
        }
        .into();
        page.changes_cursor = Some(source.cursor(binding, "changes", source.sequence as usize));
        let count = source.order.len();
        match window {
            Window::Turns { cursor } => {
                let start = cursor
                    .as_deref()
                    .map(|c| source.decode(binding, c, "turns"))
                    .transpose()?
                    .map_or(0, |c| c.position);
                for turn in source.turns.iter().skip(start).take(limits.limit) {
                    page.turns.push(turn.clone());
                    if serialized_size(&page) > limits.max_bytes.saturating_sub(256) {
                        page.turns.pop();
                        break;
                    }
                }
                if start < source.turns.len() && page.turns.is_empty() {
                    return Err(HistoryError::new(
                        "response_too_large",
                        "turn excerpt cannot fit within the serialized response budget",
                    ));
                }
                let next = start + page.turns.len();
                if next < source.turns.len() {
                    page.turns_cursor = Some(source.cursor(binding, "turns", next));
                }
            }
            Window::Search { query, cursor } => {
                if query.is_empty() || query.len() > 4096 {
                    return Err(HistoryError::new(
                        "invalid_params",
                        "search query must contain 1 to 4096 bytes",
                    ));
                }
                let fingerprint = claude::digest(query.as_bytes());
                let start = if let Some(cursor) = cursor {
                    let cursor = source.decode(binding, &cursor, "search")?;
                    if cursor.query.as_deref() != Some(&fingerprint) {
                        return Err(expired());
                    }
                    cursor.position
                } else {
                    0
                };
                let needle = query.to_lowercase();
                let mut next = start;
                for i in start..count {
                    next = i + 1;
                    let id = source.order[i].clone();
                    let message = source.message(&id)?;
                    if message.blocks.iter().any(|b| {
                        b.text
                            .as_ref()
                            .is_some_and(|t| t.to_lowercase().contains(&needle))
                            || b.label
                                .as_ref()
                                .is_some_and(|t| t.to_lowercase().contains(&needle))
                    }) {
                        page.match_message_ids.push(id);
                        if page.match_message_ids.len() >= limits.limit
                            || serialized_size(&page) > limits.max_bytes.saturating_sub(1024)
                        {
                            break;
                        }
                    }
                }
                if next < count {
                    let mut cursor = source.make_cursor(binding, "search", next);
                    cursor.query = Some(fingerprint);
                    page.search_cursor = Some(encode_cursor(&cursor));
                }
            }
            Window::Changes { cursor } => {
                let cursor = source.decode(binding, &cursor, "changes")?;
                let position = cursor.position as u64;
                if position > source.sequence
                    || source
                        .journal
                        .front()
                        .is_some_and(|c| position + 1 < c.sequence)
                {
                    return Err(expired());
                }
                let changes: Vec<_> = source
                    .journal
                    .iter()
                    .filter(|c| c.sequence > position)
                    .cloned()
                    .collect();
                let mut next = position;
                // A bounded journal can contain repeat IDs. Whole-message replacement makes replay safe.
                for change in changes.into_iter().take(limits.limit) {
                    if change.removed {
                        page.removed_message_ids.push(change.id.clone());
                    } else if source.order.contains(&change.id) {
                        let message = source.message(&change.id)?;
                        page.upserts.push(message);
                    }
                    if serialized_size(&page) > limits.max_bytes.saturating_sub(1024)
                        && page.upserts.len() + page.removed_message_ids.len() > 1
                    {
                        if change.removed {
                            page.removed_message_ids.pop();
                        } else {
                            page.upserts.pop();
                        }
                        break;
                    }
                    next = change.sequence;
                }
                let changed_ids: Vec<_> = page.upserts.iter().map(|m| m.id.clone()).collect();
                page.turns = source
                    .turns
                    .iter()
                    .filter(|t| changed_ids.contains(&t.message_id))
                    .cloned()
                    .collect();
                page.changes_cursor = Some(source.cursor(binding, "changes", next as usize));
                source.bound_messages(binding, &mut page, limits.max_bytes, None)?;
            }
            navigation => {
                let anchor = match &navigation {
                    Window::Around { message_id } => Some(message_id.clone()),
                    Window::Latest | Window::Before { .. } => Some(String::new()),
                    _ => None,
                };
                let (start, end) = match navigation {
                    Window::Latest => (count.saturating_sub(limits.limit), count),
                    Window::Before { cursor } => {
                        let end = source
                            .decode(binding, &cursor, "before")?
                            .position
                            .min(count);
                        (end.saturating_sub(limits.limit), end)
                    }
                    Window::After { cursor } => {
                        let start = source
                            .decode(binding, &cursor, "after")?
                            .position
                            .min(count);
                        (start, (start + limits.limit).min(count))
                    }
                    Window::Around { message_id } => {
                        let index = source
                            .order
                            .iter()
                            .position(|id| id == &message_id)
                            .ok_or_else(|| {
                                HistoryError::new(
                                    "message_not_found",
                                    "message is not in the current conversation branch",
                                )
                            })?;
                        let start = index.saturating_sub(limits.limit / 2);
                        (start, (start + limits.limit).min(count))
                    }
                    _ => unreachable!(),
                };
                for i in start..end {
                    let id = source.order[i].clone();
                    page.messages.push(source.message(&id)?);
                }
                // The index begins at the oldest human turn independently of the body window.
                page.turns = source.turns.iter().take(limits.limit).cloned().collect();
                if page.turns.len() < source.turns.len() {
                    page.turns_cursor = Some(source.cursor(binding, "turns", page.turns.len()));
                }
                if start > 0 {
                    page.before_cursor = Some(source.cursor(binding, "before", start));
                }
                if end < count {
                    page.after_cursor = Some(source.cursor(binding, "after", end));
                }
                let anchor = anchor.map(|id| {
                    if id.is_empty() {
                        page.messages
                            .last()
                            .map(|m| m.id.clone())
                            .unwrap_or_default()
                    } else {
                        id
                    }
                });
                source.bound_messages(binding, &mut page, limits.max_bytes, anchor.as_deref())?;
            }
        }
        if serialized_size(&page) > limits.max_bytes {
            return Err(HistoryError::new(
                "response_too_large",
                "history metadata exceeds the response budget",
            ));
        }
        source.verify_unchanged()?;
        Ok(page)
    }
    pub fn read_block(
        &mut self,
        binding: &HistoryBinding,
        block_cursor: &str,
        limits: Limits,
    ) -> Result<BlockPage, HistoryError> {
        let limits = limits.validate(false)?;
        let source = self.refresh(binding)?;
        let cursor = source.decode(binding, block_cursor, "block")?;
        let message = source.message(cursor.message.as_deref().ok_or_else(expired)?)?;
        if cursor.revision.as_deref() != Some(message.message_revision.as_str()) {
            return Err(expired());
        }
        let block = message
            .blocks
            .iter()
            .find(|b| Some(b.id.as_str()) == cursor.block.as_deref())
            .ok_or_else(expired)?;
        let text = block.text.as_deref().ok_or_else(expired)?;
        if cursor.position >= text.len()
            || !text.is_char_boundary(cursor.position)
            || cursor.slice == 0
            || cursor.slice > MAX_LINE
        {
            return Err(expired());
        }
        let mut result = BlockPage {
            binding: binding.public(),
            revision: source.revision.to_string(),
            message_id: message.id.clone(),
            message_revision: message.message_revision.clone(),
            block_id: block.id.clone(),
            kind: block.kind.clone(),
            text: String::new(),
            slice_index: cursor.slice,
            next_cursor: None,
        };
        let mut high = text.len();
        // A conservative byte allowance followed by exact encoded-size correction.
        let mut end = char_floor(
            text,
            (cursor.position + (limits.max_bytes / 6).max(1)).min(high),
        );
        loop {
            result.text = text[cursor.position..end].into();
            if end < text.len() {
                let mut next = cursor.clone();
                next.position = end;
                next.slice += 1;
                result.next_cursor = Some(encode_cursor(&next));
            } else {
                result.next_cursor = None;
            }
            if serialized_size(&result) <= limits.max_bytes.saturating_sub(512) {
                break;
            }
            high = end;
            end = char_floor(text, cursor.position + (high - cursor.position) / 2);
            if end == cursor.position {
                return Err(HistoryError::new(
                    "response_too_large",
                    "block metadata exceeds budget",
                ));
            }
        }
        source.verify_unchanged()?;
        Ok(result)
    }

    pub(crate) fn exact_human_prompt(
        &mut self,
        binding: &HistoryBinding,
        message_id: &str,
        message_revision: &str,
    ) -> Result<ExactHumanPrompt, HistoryError> {
        let source = self.refresh(binding)?;
        if source.unsupported || source.partial {
            return Err(HistoryError::new("jump_unavailable","saved transcript includes unsupported or pending records; full prompt identity cannot be established"));
        }
        let message = source.message(message_id)?;
        if message.message_revision != message_revision {
            return Err(HistoryError::new(
                "message_changed",
                "saved message revision changed; reload history",
            ));
        }
        let text = exact_prompt(&message).ok_or_else(|| {
            HistoryError::new(
                "jump_unavailable",
                "only complete human text prompts can be matched to terminal rows",
            )
        })?;
        validate_prompt(text)?;
        let fingerprint = claude::digest(text.as_bytes());
        // Includes abandoned branch entries: a single surviving terminal occurrence
        // cannot identify repeated human prompts whose earlier occurrence was evicted.
        if source
            .entries
            .values()
            .filter(|entry| {
                entry.human && entry.prompt_fingerprint.as_deref() == Some(&fingerprint)
            })
            .count()
            != 1
        {
            return Err(HistoryError::new(
                "jump_ambiguous",
                "repeated saved human prompts cannot be uniquely associated with terminal rows",
            ));
        }
        let human_index = source
            .order
            .iter()
            .position(|id| id == message_id)
            .ok_or_else(|| {
                HistoryError::new(
                    "message_changed",
                    "human message is no longer on the current branch",
                )
            })?;
        let following = source
            .order
            .iter()
            .skip(human_index + 1)
            .take_while(|id| source.entries[*id].turn_id == message.turn_id)
            .cloned()
            .collect::<Vec<_>>();
        let mut assistant_line = None;
        for id in following {
            let next = source.message(&id)?;
            if next.role == "assistant" {
                assistant_line = Some(assistant_context_line(&next)?);
                break;
            }
        }
        let assistant_line = assistant_line.ok_or_else(|| {
            HistoryError::new(
                "jump_unavailable",
                "no supported assistant reply can verify this rendered human exchange",
            )
        })?;
        source.verify_unchanged()?;
        Ok(ExactHumanPrompt {
            binding: binding.clone(),
            message_id: message.id.clone(),
            message_revision: message.message_revision.clone(),
            text: text.into(),
            assistant_line,
            guard: JumpSourceGuard {
                revision: source.jump_revision.clone(),
                expected: source.jump_revision.load(Ordering::Acquire),
            },
        })
    }
    pub(crate) fn validate_jump_source(
        &mut self,
        prompt: &ExactHumanPrompt,
    ) -> Result<(), HistoryError> {
        let source = self.refresh(&prompt.binding)?;
        prompt.guard.validate()?;
        if !Arc::ptr_eq(&source.jump_revision, &prompt.guard.revision) {
            return Err(HistoryError::new(
                "source_changed",
                "saved source identity was replaced",
            ));
        }
        source.verify_unchanged()
    }
    fn refresh(&mut self, binding: &HistoryBinding) -> Result<&mut Source, HistoryError> {
        let key = format!("{}:{}", binding.session_id, binding.generation);
        self.lru.retain(|k| k != &key);
        self.lru.push_back(key.clone());
        if !self.sources.contains_key(&key) && self.sources.len() >= MAX_SOURCES {
            if let Some(old) = self.lru.pop_front() {
                if let Some(source) = self.sources.remove(&old) {
                    source.jump_revision.store(u64::MAX, Ordering::Release);
                }
            }
        }
        let source = self.sources.entry(key).or_insert_with(|| Source {
            epoch: next_epoch(),
            ..Source::default()
        });
        if source.jump_revision.load(Ordering::Acquire) == u64::MAX {
            *source = Source {
                epoch: next_epoch(),
                ..Source::default()
            };
        }
        if let Err(error) = source.refresh(binding) {
            source.jump_revision.store(u64::MAX, Ordering::Release);
            return Err(error);
        }
        Ok(source)
    }
}
impl Source {
    fn refresh(&mut self, binding: &HistoryBinding) -> Result<(), HistoryError> {
        self.session_id = binding.session_id.clone();
        let path = binding.transcript_path.canonicalize()?;
        let metadata = std::fs::metadata(&path)?;
        if !metadata.is_file() {
            return Err(HistoryError::new(
                "unsupported_schema",
                "transcript is not a regular file",
            ));
        }
        let mut file = crate::platform::open_regular_file_readonly(&path)?;
        let metadata = file.metadata()?;
        let identity = Some(crate::platform::regular_file_identity(&file)?);
        if !metadata.is_file() {
            return Err(HistoryError::new(
                "unsupported_schema",
                "transcript is not a regular file",
            ));
        }
        let modified = metadata.modified().ok();
        let created = metadata.created().ok();
        let len = metadata.len();
        if self.path == path
            && self.identity == identity
            && self.created == created
            && self.modified == modified
            && self.length == len
        {
            return Ok(());
        }
        let mut rewrite = !self.path.as_os_str().is_empty()
            && (self.path != path
                || self.identity != identity
                || self.created != created
                || len < self.complete_offset);
        if !rewrite && self.complete_offset > 0 {
            let mut hash = Sha256::new();
            let mut prefix = (&mut file).take(self.complete_offset);
            let mut buffer = [0u8; 65536];
            loop {
                let n = prefix.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            rewrite = format!("{:x}", hash.finalize()) != self.prefix_hash;
        }
        if rewrite {
            self.entries.clear();
            self.parent_links.clear();
            self.order.clear();
            self.turns.clear();
            self.complete_offset = 0;
            self.epoch = next_epoch();
            self.journal.clear();
            self.cache.clear();
            self.cache_bytes = 0;
            self.unsupported = false;
        }
        let old: HashMap<_, _> = self
            .order
            .iter()
            .filter_map(|id| {
                self.entries
                    .get(id)
                    .map(|e| (id.clone(), e.revision.clone()))
            })
            .collect();
        file.seek(SeekFrom::Start(self.complete_offset))?;
        let mut reader = BufReader::new(file);
        let mut offset = self.complete_offset;
        self.partial = false;
        loop {
            let mut line = Vec::new();
            let mut oversized = false;
            let mut consumed = 0usize;
            let mut complete = false;
            loop {
                let available = reader.fill_buf()?;
                if available.is_empty() {
                    break;
                }
                let n = available
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(available.len(), |i| i + 1);
                if line.len() + n <= MAX_LINE && !oversized {
                    line.extend_from_slice(&available[..n]);
                } else {
                    oversized = true;
                    line.clear();
                }
                complete = available[n - 1] == b'\n';
                consumed += n;
                reader.consume(n);
                if complete {
                    break;
                }
            }
            if consumed == 0 {
                break;
            }
            if !complete {
                self.partial = true;
                break;
            }
            if oversized {
                self.unsupported = true;
            } else if !line.iter().all(u8::is_ascii_whitespace) {
                match claude::parse_record(&line, &binding.session_id) {
                    Ok(claude::ParsedRecord {
                        message: Some(message),
                        ..
                    }) => {
                        if self.parent_links.remove(&message.id).is_some() {
                            self.unsupported = true;
                        }
                        let entry = Entry {
                            offset,
                            first_offset: self
                                .entries
                                .get(&message.id)
                                .map_or(offset, |entry| entry.first_offset),
                            compact: message.status == "compaction_notice",
                            len: consumed,
                            id: message.id.clone(),
                            parent: message.parent_id.clone(),
                            revision: message.message_revision.clone(),
                            human: message.origin == "human",
                            prompt_fingerprint: exact_prompt(&message)
                                .map(|text| claude::digest(text.as_bytes())),
                            preview: message
                                .blocks
                                .iter()
                                .filter_map(|b| b.text.as_deref().or(b.label.as_deref()))
                                .collect::<Vec<_>>()
                                .join(" ")
                                .chars()
                                .take(160)
                                .collect(),
                            timestamp: message.timestamp.clone(),
                            order: 0,
                            turn_id: String::new(),
                        };
                        if !self.order.contains(&entry.id) {
                            self.order.push(entry.id.clone());
                        }
                        self.entries.insert(entry.id.clone(), entry);
                    }
                    Ok(claude::ParsedRecord {
                        parent_link: Some(claude::ParentLink { id, parent }),
                        ..
                    }) => {
                        if self.entries.contains_key(&id) {
                            self.unsupported = true;
                        } else {
                            self.parent_links.insert(id, parent);
                        }
                    }
                    Ok(_) => {}
                    Err(error) if error.code == "session_changed" => return Err(error),
                    Err(_) => {
                        self.unsupported = true;
                    }
                }
            }
            offset += consumed as u64;
        }
        self.complete_offset = offset;
        self.path = path;
        self.modified = modified;
        self.identity = identity;
        self.created = created;
        self.length = len;
        let mut prefix = crate::platform::open_regular_file_readonly(&self.path)?.take(offset);
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = prefix.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        self.prefix_hash = format!("{:x}", hash.finalize());
        self.select_main_branch();
        self.turns.clear();
        let mut turn_id = String::new();
        for (position, id) in self.order.iter().enumerate() {
            let entry = self.entries.get_mut(id).unwrap();
            entry.order = position + 1;
            if entry.human {
                turn_id = format!("turn:{id}");
                self.turns.push(ConversationTurn {
                    id: turn_id.clone(),
                    message_id: id.clone(),
                    ordinal: self.turns.len() + 1,
                    preview: entry.preview.clone(),
                    timestamp: entry.timestamp.clone(),
                });
            }
            entry.turn_id = turn_id.clone();
        }
        let current: HashMap<_, _> = self
            .order
            .iter()
            .map(|id| (id.clone(), self.entries[id].revision.clone()))
            .collect();
        let removed: Vec<_> = old
            .keys()
            .filter(|id| !current.contains_key(*id))
            .cloned()
            .collect();
        let changed: Vec<_> = self
            .order
            .iter()
            .filter(|id| old.get(*id) != current.get(*id))
            .cloned()
            .collect();
        if !removed.is_empty() || !changed.is_empty() || rewrite {
            self.revision += 1;
        }
        for id in removed {
            self.record_change(id, true);
        }
        for id in changed {
            self.record_change(id, false);
        }
        // Source guards cover physical transcript changes, including completeness-only
        // updates; the public semantic revision remains independent.
        self.jump_revision.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn verify_unchanged(&self) -> Result<(), HistoryError> {
        let file = crate::platform::open_regular_file_readonly(&self.path)?;
        let metadata = file.metadata()?;
        if Some(crate::platform::regular_file_identity(&file)?) != self.identity
            || !metadata.is_file()
            || metadata.len() != self.length
            || metadata.modified().ok() != self.modified
            || metadata.created().ok() != self.created
        {
            return Err(expired());
        }
        Ok(())
    }
    fn select_main_branch(&mut self) {
        // Claude parent chains identify the selected main branch. Parentless synthetic/older
        // records retain file order; disconnected compact boundaries preserve available context.
        let Some(tip) = self.order.last().cloned() else {
            return;
        };
        if self.entries[&tip].parent.is_none() {
            return;
        }
        let mut ancestors = std::collections::HashSet::new();
        let mut at = Some(tip);
        while let Some(id) = at {
            if !ancestors.insert(id.clone()) {
                self.unsupported = true;
                break;
            }
            let parent = match self.entries.get(&id) {
                Some(entry) => entry.parent.clone(),
                None => self.parent_links.get(&id).cloned().flatten(),
            };
            at = parent.filter(|parent| {
                self.entries.contains_key(parent) || self.parent_links.contains_key(parent)
            });
        }
        if ancestors.len() > 1 {
            let compact_offset = ancestors
                .iter()
                .filter_map(|id| self.entries.get(id))
                .filter(|entry| entry.compact)
                .map(|entry| entry.first_offset)
                .min();
            let retained_prefix: std::collections::HashSet<_> = self
                .order
                .iter()
                .filter(|id| {
                    compact_offset.is_some_and(|offset| self.entries[*id].first_offset < offset)
                })
                .cloned()
                .collect();
            let mut order: Vec<_> = self
                .entries
                .values()
                .filter(|entry| {
                    ancestors.contains(&entry.id) || retained_prefix.contains(&entry.id)
                })
                .collect();
            order.sort_by_key(|entry| entry.first_offset);
            self.order = order.into_iter().map(|entry| entry.id.clone()).collect();
        }
    }
    fn record_change(&mut self, id: String, removed: bool) {
        self.cache.retain(|(cached, _, _)| cached != &id);
        self.cache_bytes = self.cache.iter().map(|(_, _, n)| n).sum();
        self.sequence += 1;
        self.journal.push_back(Change {
            sequence: self.sequence,
            id,
            removed,
        });
        while self.journal.len() > JOURNAL {
            self.journal.pop_front();
        }
    }
    fn message(&mut self, id: &str) -> Result<ConversationMessage, HistoryError> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| HistoryError::new("message_not_found", "message is unavailable"))?;
        if !self.order.contains(&entry.id) {
            return Err(HistoryError::new(
                "message_not_found",
                "message is outside the current branch",
            ));
        }
        let order = entry.order;
        let turn_id = entry.turn_id.clone();
        let mut message =
            if let Some(index) = self.cache.iter().position(|(cached, _, _)| cached == id) {
                let entry = self.cache.remove(index).unwrap();
                let result = entry.1.clone();
                self.cache.push_back(entry);
                result
            } else {
                let entry = self.entries.get(id).ok_or_else(expired)?;
                let mut file = crate::platform::open_regular_file_readonly(&self.path)?;
                if Some(crate::platform::regular_file_identity(&file)?) != self.identity {
                    return Err(expired());
                }
                file.seek(SeekFrom::Start(entry.offset))?;
                let mut line = vec![0; entry.len];
                file.read_exact(&mut line)?;
                let message = claude::normalize(&line, self.session_from_file_entry()?)?
                    .ok_or_else(expired)?;
                if message.message_revision != entry.revision {
                    return Err(expired());
                }
                let size = serialized_size(&message);
                if size <= BODY_CACHE {
                    while self.cache_bytes + size > BODY_CACHE {
                        if let Some((_, _, n)) = self.cache.pop_front() {
                            self.cache_bytes -= n;
                        } else {
                            break;
                        }
                    }
                    self.cache_bytes += size;
                    self.cache.push_back((id.into(), message.clone(), size));
                }
                message
            };
        message.order = order;
        message.turn_id = turn_id;
        Ok(message)
    }
    fn session_from_file_entry(&self) -> Result<&str, HistoryError> {
        Ok(&self.session_id)
    }
    fn make_cursor(&self, binding: &HistoryBinding, kind: &str, position: usize) -> Cursor {
        Cursor {
            generation: binding.generation.clone(),
            epoch: self.epoch,
            kind: kind.into(),
            position,
            message: match kind {
                "before" => self.order.get(position).cloned(),
                "after" => position
                    .checked_sub(1)
                    .and_then(|p| self.order.get(p))
                    .cloned(),
                _ => None,
            },
            revision: None,
            block: None,
            slice: 0,
            query: None,
        }
    }
    fn cursor(&self, binding: &HistoryBinding, kind: &str, position: usize) -> String {
        encode_cursor(&self.make_cursor(binding, kind, position))
    }
    fn decode(
        &self,
        binding: &HistoryBinding,
        cursor: &str,
        kind: &str,
    ) -> Result<Cursor, HistoryError> {
        if cursor.len() > 8192 {
            return Err(expired());
        }
        let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| expired())?;
        let mut value: Cursor = serde_json::from_slice(&bytes).map_err(|_| expired())?;
        if value.generation != binding.generation || value.epoch != self.epoch || value.kind != kind
        {
            return Err(expired());
        }
        if matches!(kind, "before" | "after") {
            let anchor = value.message.as_ref().ok_or_else(expired)?;
            let position = self
                .order
                .iter()
                .position(|id| id == anchor)
                .ok_or_else(expired)?;
            value.position = position + usize::from(kind == "after");
        }
        Ok(value)
    }
    fn bound_messages(
        &self,
        binding: &HistoryBinding,
        page: &mut ConversationPage,
        budget: usize,
        anchor: Option<&str>,
    ) -> Result<(), HistoryError> {
        // Reserve for outer success/id/type framing as well as cursor strings.
        while serialized_size(page) > budget.saturating_sub(512) && page.turns.len() > 1 {
            page.turns.pop();
            page.turns_cursor = Some(self.cursor(binding, "turns", page.turns.len()));
        }
        let total = page.messages.len() + page.upserts.len();
        let per_block = (budget.saturating_sub(
            serialized_size(&ConversationPage::unavailable(
                Some(binding.public()),
                "ready",
            )) + 2048,
        ) / total.max(1)
            / 6)
        .max(16);
        for message in page.messages.iter_mut().chain(page.upserts.iter_mut()) {
            let blocks = message.blocks.len().max(1);
            for block in &mut message.blocks {
                if let Some(text) = block.text.as_mut() {
                    let end = char_floor(text, (per_block / blocks).max(1).min(text.len()));
                    if end < text.len() {
                        let mut cursor = self.make_cursor(binding, "block", end);
                        cursor.message = Some(message.id.clone());
                        cursor.revision = Some(message.message_revision.clone());
                        cursor.block = Some(block.id.clone());
                        cursor.slice = 1;
                        text.truncate(end);
                        block.truncated = true;
                        block.block_cursor = Some(encode_cursor(&cursor));
                    }
                }
            }
        }
        // Metadata may itself exceed a small requested budget. Return fewer whole messages,
        // preserving progress through the chronological cursor; never drop the only message.
        while serialized_size(page) > budget.saturating_sub(512) && page.messages.len() > 1 {
            let anchor_position = anchor
                .and_then(|id| page.messages.iter().position(|m| m.id == id))
                .unwrap_or(0);
            if anchor_position > page.messages.len() / 2 {
                page.messages.remove(0);
                let first = page.messages.first().unwrap();
                page.before_cursor = Some(self.cursor(binding, "before", first.order - 1));
            } else {
                let removed = page.messages.pop().unwrap();
                page.after_cursor = Some(self.cursor(binding, "after", removed.order - 1));
            }
        }
        if serialized_size(page) > budget.saturating_sub(512) {
            return Err(HistoryError::new(
                "response_too_large",
                "message metadata exceeds response budget",
            ));
        }
        Ok(())
    }
}
fn serialized_size<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}
fn expired() -> HistoryError {
    HistoryError::new(
        "cursor_expired",
        "saved conversation cursor is no longer valid",
    )
}
fn encode_cursor(cursor: &Cursor) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor).unwrap())
}
fn char_floor(text: &str, mut position: usize) -> usize {
    while !text.is_char_boundary(position) {
        position -= 1;
    }
    position
}

fn next_epoch() -> u64 {
    static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Drop for ConversationReader {
    fn drop(&mut self) {
        for source in self.sources.values() {
            source.jump_revision.store(u64::MAX, Ordering::Release);
        }
    }
}
