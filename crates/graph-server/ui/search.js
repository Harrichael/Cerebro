import { useState, useEffect, useMemo, useRef } from 'react';
import { html, fetchJson } from './common.js';

const DEBOUNCE_MS = 120;
const GROUP_LABEL = { symbol: 'Symbols', file: 'Files', path: 'Paths', content: 'Lines' };
const MORE_LABEL = { symbol: 'symbols', file: 'files', path: 'paths', content: 'lines' };
const KIND_ORDER = ['symbol', 'file', 'path', 'content'];

// Hidden ids and the test filter are client-only state the server has no
// notion of; apply them to each hit's file id the same way the diagram does.
const isHidden = (graph, id, hiddenIds, showTests) => {
  const node = graph.nodes[id];
  if (!node) return true;
  if (!showTests && node.is_test) return true;
  for (let cur = id; cur != null; cur = graph.nodes[cur].parent) if (hiddenIds.has(cur)) return true;
  return false;
};

// start/end are UTF-16 offsets, same units as JS string indices, so slicing
// needs no conversion. Only the leading-whitespace trim shifts them, and a
// naive shift can go negative, which slice() would read as "from the end".
const trimLead = (text, start, end) => {
  const cut = /^\s*/.exec(text)[0].length;
  const s = Math.max(0, start - cut);
  return { text: text.slice(cut), start: s, end: Math.max(s, end - cut) };
};

const mark = (text, start, end) => html`${text.slice(0, start)}<b>${text.slice(start, end)}</b>${text.slice(end)}`;

export function Search({ graph, showTests, hiddenIds, onPick }) {
  const [q, setQ] = useState('');
  const [open, setOpen] = useState(false);
  const [cursor, setCursor] = useState(0);
  const [result, setResult] = useState({ hits: [], more: {}, error: null });
  const inputRef = useRef(null);
  const listRef = useRef(null);
  const reqRef = useRef(0);
  const graphRef = useRef(graph); graphRef.current = graph;

  useEffect(() => {
    const onKey = (e) => {
      const tag = document.activeElement?.tagName;
      if (e.key === '/' && tag !== 'INPUT' && tag !== 'TEXTAREA' && !e.metaKey && !e.ctrlKey) { e.preventDefault(); inputRef.current?.focus(); }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  // Debounced, request-id-guarded fetch. The previous result is left in state
  // until a newer request resolves, so the list never flickers to "no match"
  // while a query is in flight. A result from another generation than the
  // graph's is dropped too: ids are dense, so its hits would name other
  // entities, which isHidden cannot tell apart.
  useEffect(() => {
    const needle = q.trim();
    const id = ++reqRef.current;
    if (!needle) { setResult({ hits: [], more: {}, error: null }); return; }
    const t = setTimeout(() => {
      fetchJson(`./search?q=${encodeURIComponent(needle)}`)
        .then((res) => { if (reqRef.current === id && res.generation === graphRef.current?.generation) setResult({ hits: res.hits, more: res.more, error: null }); })
        .catch((e) => { if (reqRef.current === id) setResult({ hits: [], more: {}, error: e.message }); });
    }, DEBOUNCE_MS);
    return () => clearTimeout(t);
    // Re-run on a new generation: the listed hits carry ids from the old one.
  }, [q, graph?.generation]);

  const rows = useMemo(
    () => (graph ? result.hits.filter((h) => !isHidden(graph, h.id, hiddenIds, showTests)) : []),
    [graph, result.hits, hiddenIds, showTests],
  );

  useEffect(() => { setCursor(0); }, [q]);
  useEffect(() => { listRef.current?.querySelector('.on')?.scrollIntoView({ block: 'nearest' }); }, [cursor]);

  const pick = (r) => { setOpen(false); onPick(r.id, r.kind === 'content' ? r.line : undefined); };
  const onKeyDown = (e) => {
    if (e.key === 'ArrowDown') { e.preventDefault(); setOpen(true); setCursor((c) => Math.min(rows.length - 1, c + 1)); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); setCursor((c) => Math.max(0, c - 1)); }
    else if (e.key === 'Enter') { if (rows[cursor]) pick(rows[cursor]); }
    else if (e.key === 'Escape') { setQ(''); setOpen(false); e.currentTarget.blur(); }
  };

  const rowBody = (r) => {
    if (r.kind === 'content') {
      const t = trimLead(r.text, r.start, r.end);
      return html`<span class="where">${r.path}:${r.line + 1}</span><span class="text">${mark(t.text, t.start, t.end)}</span>`;
    }
    if (r.kind === 'file') return html`<span class="name">${mark(r.text, r.start, r.end)}</span><span class="path">${r.path}</span>`;
    return html`<span class="name">${mark(r.text, r.start, r.end)}</span>`;
  };

  // Group headers live between rows, not in `rows`, so the cursor (indexed
  // into `rows`) always lands on a real hit.
  let prevKind = null;
  const items = [];
  rows.forEach((r, i) => {
    if (r.kind !== prevKind) { items.push(html`<div key=${`head-${r.kind}`} class="search-head">${GROUP_LABEL[r.kind]}</div>`); prevKind = r.kind; }
    items.push(html`<div key=${`${r.kind}:${r.id}:${r.line ?? ''}:${r.start}`} class=${'search-row' + (i === cursor ? ' on' : '')}
      onMouseEnter=${() => setCursor(i)} onClick=${() => pick(r)}>
      <span class="kind">${r.kind}</span>${rowBody(r)}
    </div>`);
  });

  return html`<div class="search">
    <input ref=${inputRef} type="search" placeholder="search files, paths, content  /" value=${q}
      title=${'Terms separated by spaces must all match. Tag a term with file:, path: or content: to pin it to one kind, e.g. class file:resolver. "Quote a phrase" to keep its spaces.'}
      onInput=${(e) => { setQ(e.target.value); setOpen(true); }} onFocus=${() => setOpen(true)}
      onBlur=${() => setOpen(false)} onKeyDown=${onKeyDown} />
    ${open && q.trim() && html`<div class="search-list" ref=${listRef} onMouseDown=${(e) => e.preventDefault()}>
      ${result.error && html`<div class="search-empty">${result.error}</div>`}
      ${!result.error && items}
      ${!result.error && !rows.length && html`<div class="search-empty">no match</div>`}
      ${!result.error && KIND_ORDER.map((k) => result.more?.[k] > 0 && html`<div key=${`more-${k}`} class="search-empty">${result.more[k]} more ${MORE_LABEL[k]}…</div>`)}
    </div>`}
  </div>`;
}
