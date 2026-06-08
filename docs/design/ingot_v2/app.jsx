/* app.jsx — orchestration: imports, state, keyboard culling, ingest simulation, tweaks. */

const TWEAK_DEFAULTS = /*EDITMODE-BEGIN*/{
  "layout": "classic",
  "accent": "#3b82f6",
  "thumb": 188,
  "autoAdvance": true
}/*EDITMODE-END*/;

const ACCENTS = {
  '#3b82f6': { a2: '#60a5fa', dim: '#1e3a6b' },
  '#f59e0b': { a2: '#fbbf24', dim: '#4a3410' },
  '#10b981': { a2: '#34d399', dim: '#0f3d2e' },
  '#a78bfa': { a2: '#c4b5fd', dim: '#312a52' },
};

function filterFn(f) {
  if (f === 'all') return () => true;
  if (f === 'pick') return (p) => p.flag === 'accept';
  if (f === 'reject') return (p) => p.flag === 'reject';
  if (f === 'none') return (p) => p.flag === 'none';
  if (f.startsWith('rup:')) return (p) => p.rating >= +f.slice(4);
  if (f.startsWith('rdn:')) return (p) => p.rating <= +f.slice(4);
  if (f[0] === 'r' && f[1] !== 'u' && f[1] !== 'd' && f[1] !== 'e') return (p) => p.rating >= +f.slice(1);
  if (f.startsWith('cl:')) return (p) => p.cluster === f.slice(3);
  return () => true;
}

function fmtImportName(d) {
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} · ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function App() {
  const [t, setTweak] = useTweaks(TWEAK_DEFAULTS);
  const data = window.INGOT_DATA;
  const BASE = data.PHOTOS;
  const RATE = { 16: 0.85, 8: 0.42, 4: 0.2 };

  // seed two already-imported sessions so the app opens with content
  const seed = useMemo(() => {
    let nid = 1;
    const clone = (impId, list, done) => list.map((p) => ({
      ...p, id: nid++, importId: impId,
      thumbReady: done, ingest: done ? 'done' : 'pending', copyPct: done ? 100 : 0,
    }));
    const A = clone('imp_a', BASE.filter((p) => ['wall', 'tower', 'oak'].includes(p.cluster)), true);
    const B = clone('imp_b', BASE.filter((p) => ['harbor', 'cafe', 'street'].includes(p.cluster)), true);
    return { photos: [...A, ...B], next: nid };
  }, []);

  const [photos, setPhotos] = useState(seed.photos);
  const nextId = useRef(seed.next);
  const [imports, setImports] = useState([
    { id: 'imp_a', name: '2026-05-10 · 09:14', targets: data.TARGETS.slice(0, 2), hier: 'YYYY/YYYY-MM-DD' },
    { id: 'imp_b', name: '2026-05-12 · 14:02', targets: data.TARGETS.slice(0, 2), hier: 'YYYY/YYYY-MM-DD' },
  ]);
  const [activeImport, setActiveImport] = useState('all');
  const [config, setConfig] = useState({ targets: data.TARGETS.slice(0, 2), hier: 'YYYY/YYYY-MM-DD' });
  const [ingestImportId, setIngestImportId] = useState(null);

  const [importerOpen, setImporterOpen] = useState(false);
  const [view, setView] = useState('grid');
  const [filter, setFilter] = useState('all');
  const [focusId, setFocusId] = useState(1);
  const [selected, setSelected] = useState(() => new Set([1]));
  const [surveyIds, setSurveyIds] = useState([]);
  const anchorRef = useRef(1);
  const [modal, setModal] = useState(null);
  const [toast, setToast] = useState('');
  const [sensitivity, setSensitivity] = useState(20);
  const [ing, setIng] = useState({ running: false, stats: {}, eta: 0, done: 0, total: 0 });

  const copied = useRef({});
  const stageRef = useRef(null);
  const tweaks = ACCENTS[t.accent] || ACCENTS['#3b82f6'];

  useEffect(() => {
    const r = document.documentElement.style;
    r.setProperty('--accent', t.accent);
    r.setProperty('--accent-2', tweaks.a2);
    r.setProperty('--accent-dim', tweaks.dim);
  }, [t.accent]);

  /* ---------- scoping by active import ---------- */
  const scoped = useMemo(() => activeImport === 'all' ? photos : photos.filter((p) => p.importId === activeImport), [photos, activeImport]);
  const ordered = useMemo(() => scoped.slice().sort((a, b) => a.captured - b.captured), [scoped]);
  const list = useMemo(() => ordered.filter(filterFn(filter)), [ordered, filter]);
  const byId = useMemo(() => { const m = {}; photos.forEach((p) => (m[p.id] = p)); return m; }, [photos]);
  const byIdRef = useRef({});
  byIdRef.current = byId;
  const focus = byId[focusId];

  useEffect(() => {
    if (list.length && !list.some((p) => p.id === focusId)) { setFocusId(list[0].id); }
  }, [list, focusId]);

  // keep selection within the visible list; default to focus
  useEffect(() => {
    setSelected((sel) => {
      const valid = new Set([...sel].filter((id) => list.some((p) => p.id === id)));
      if (valid.size === 0 && list.some((p) => p.id === focusId)) valid.add(focusId);
      return valid;
    });
  }, [list]);

  const activeObj = imports.find((i) => i.id === activeImport);
  const barTargets = ingestImportId
    ? config.targets
    : (activeObj ? activeObj.targets : config.targets);

  const counts = {
    pick: scoped.filter((p) => p.flag === 'accept').length,
    reject: scoped.filter((p) => p.flag === 'reject').length,
    rated: scoped.filter((p) => p.rating > 0).length,
  };
  const clusterCounts = data.CLUSTERS
    .map((c) => ({ key: c.key, name: c.name, n: scoped.filter((p) => p.cluster === c.key).length }))
    .filter((c) => c.n > 0);
  const importList = imports.map((i) => ({ ...i, n: photos.filter((p) => p.importId === i.id).length }));

  /* ---------- selection ---------- */
  const selectPhoto = useCallback((id, mods) => {
    setFocusId(id);
    setSelected((sel) => {
      const next = new Set(sel);
      if (mods && mods.shift) {
        const a = anchorRef.current;
        const ia = list.findIndex((p) => p.id === a);
        const ib = list.findIndex((p) => p.id === id);
        if (ia >= 0 && ib >= 0) {
          const [lo, hi] = ia < ib ? [ia, ib] : [ib, ia];
          if (!(mods.ctrl || mods.meta)) next.clear();
          for (let i = lo; i <= hi; i++) next.add(list[i].id);
        } else { next.clear(); next.add(id); }
        return next;
      }
      if (mods && (mods.ctrl || mods.meta)) {
        if (next.has(id)) next.delete(id); else next.add(id);
        anchorRef.current = id;
        if (next.size === 0) next.add(id);
        return next;
      }
      anchorRef.current = id;
      return new Set([id]);
    });
  }, [list]);

  const targetIds = useCallback(() => (selected.size ? [...selected] : (focus ? [focus.id] : [])), [selected, focus]);
  const clearSel = useCallback(() => { setSelected(new Set([focusId])); anchorRef.current = focusId; }, [focusId]);
  const selectAll = useCallback(() => { setSelected(new Set(list.map((p) => p.id))); }, [list]);

  /* ---------- actions (batch-aware) ---------- */
  const setFlag = useCallback((id, flag) => {
    const ids = (selected.has(id) && selected.size > 1) ? [...selected] : [id];
    const allSame = ids.every((x) => { const p = byIdRef.current[x]; return p && p.flag === flag; });
    setPhotos((ps) => ps.map((p) => ids.includes(p.id) ? { ...p, flag: allSame ? 'none' : flag } : p));
  }, [selected]);
  const setRating = useCallback((id, r) => {
    const ids = (selected.has(id) && selected.size > 1) ? [...selected] : [id];
    setPhotos((ps) => ps.map((p) => ids.includes(p.id) ? { ...p, rating: r } : p));
  }, [selected]);
  const renameImport = useCallback((id, name) => {
    setImports((xs) => xs.map((x) => x.id === id ? { ...x, name } : x));
  }, []);

  const advance = useCallback((dir, e) => {
    setFocusId((cur) => {
      const i = list.findIndex((p) => p.id === cur);
      if (i < 0) return list.length ? list[0].id : cur;
      const n = Math.max(0, Math.min(list.length - 1, i + dir));
      const nid = list[n] ? list[n].id : cur;
      if (e && e.shiftKey) {
        setSelected((sel) => { const s = new Set(sel); s.add(nid); s.add(cur); return s; });
      } else {
        setSelected(new Set([nid])); anchorRef.current = nid;
      }
      return nid;
    });
  }, [list]);

  const moveRow = useCallback((dir, e) => {
    const stage = stageRef.current;
    let cols = 5;
    if (stage) cols = Math.max(1, Math.floor((stage.clientWidth - 28) / (t.thumb + 12)));
    setFocusId((cur) => {
      const i = list.findIndex((p) => p.id === cur);
      if (i < 0) return cur;
      const n = Math.max(0, Math.min(list.length - 1, i + dir * cols));
      const nid = list[n] ? list[n].id : cur;
      if (e && e.shiftKey) {
        setSelected((sel) => { const s = new Set(sel); s.add(nid); s.add(cur); return s; });
      } else {
        setSelected(new Set([nid])); anchorRef.current = nid;
      }
      return nid;
    });
  }, [list, t.thumb]);

  /* ---------- single-target setters (for Survey per-cell controls) ---------- */
  const setFlagOne = useCallback((id, flag) => {
    setPhotos((ps) => ps.map((p) => p.id === id ? { ...p, flag: p.flag === flag ? 'none' : flag } : p));
  }, []);
  const setRatingOne = useCallback((id, r) => {
    setPhotos((ps) => ps.map((p) => p.id === id ? { ...p, rating: r } : p));
  }, []);

  /* ---------- survey / compare ---------- */
  const openSurvey = useCallback(() => {
    const ids = selected.size > 1 ? [...selected] : list.map((p) => p.id).slice(0, 9);
    const ordered2 = list.filter((p) => ids.includes(p.id)).map((p) => p.id);
    if (ordered2.length) { setSurveyIds(ordered2); setView('survey'); }
  }, [selected, list]);
  const removeFromSurvey = useCallback((id) => {
    setSurveyIds((xs) => { const n = xs.filter((x) => x !== id); if (n.length === 0) setView('grid'); return n; });
  }, []);

  /* ---------- keyboard ---------- */
  useEffect(() => {
    const onKey = (e) => {
      if (importerOpen) { if (e.key === 'Escape') setImporterOpen(false); return; }
      if (e.target && (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA')) return;
      if (modal) { if (e.key === 'Escape') setModal(null); return; }
      const k = e.key;
      if ((e.ctrlKey || e.metaKey) && (k === 'a' || k === 'A')) { e.preventDefault(); selectAll(); return; }
      if (k === 'Escape') { if (view === 'loupe' || view === 'survey') setView('grid'); else clearSel(); return; }
      // view switches
      if (k === 'g' || k === 'G') { setView('grid'); return; }
      if (k === 'e' || k === 'E') { setView('loupe'); return; }
      if (k === 'l' || k === 'L') { setView('clusters'); return; }
      if (k === 'n' || k === 'N') { openSurvey(); return; }
      if (k === 'ArrowRight') { e.preventDefault(); advance(1, e); return; }
      if (k === 'ArrowLeft') { e.preventDefault(); advance(-1, e); return; }
      if (k === 'ArrowDown' && view === 'grid') { e.preventDefault(); moveRow(1, e); return; }
      if (k === 'ArrowUp' && view === 'grid') { e.preventDefault(); moveRow(-1, e); return; }
      if (k === 'Enter') { setView('loupe'); return; }
      if (!focus) return;
      const single = view === 'survey';
      const flagFn = single ? setFlagOne : setFlag;
      const rateFn = single ? setRatingOne : setRating;
      if (k === 'x' || k === 'X') { flagFn(focus.id, 'reject'); if (t.autoAdvance && selected.size <= 1 && !single) advance(1); return; }
      if (k === 'p' || k === 'P' || k === ' ') { e.preventDefault(); flagFn(focus.id, 'accept'); if (t.autoAdvance && selected.size <= 1 && !single) advance(1); return; }
      if (k >= '1' && k <= '5') { rateFn(focus.id, +k); return; }
      if (k === '0' || k === '`') { rateFn(focus.id, 0); return; }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [importerOpen, modal, view, focus, advance, moveRow, setFlag, setRating, setFlagOne, setRatingOne, t.autoAdvance, selectAll, clearSel, openSurvey, selected]);

  /* ---------- start a new import (from "+ Import") ---------- */
  const startImport = useCallback((cfg) => {
    const id = 'imp_' + Date.now();
    const name = fmtImportName(new Date());
    const fresh = BASE.map((p) => ({
      ...p, id: nextId.current++, importId: id,
      thumbReady: false, ingest: 'pending', copyPct: 0, flag: 'none', rating: 0,
    }));
    copied.current = {}; cfg.targets.forEach((tg) => (copied.current[tg.id] = 0));
    setImports((xs) => [...xs, { id, name, targets: cfg.targets, hier: cfg.hier }]);
    setPhotos((ps) => [...ps, ...fresh]);
    setConfig(cfg);
    setActiveImport(id);
    setIngestImportId(id);
    setImporterOpen(false);
    setView('grid');
    setFilter('all');
    setFocusId(fresh[0].id);
    setIng({ running: true, stats: {}, eta: 0, done: 0, total: fresh.length });
  }, []);

  /* ---------- ingest simulation (scoped to the importing session) ---------- */
  useEffect(() => {
    if (!ingestImportId) return;
    const targets = config.targets;
    const id = setInterval(() => {
      setPhotos((ps) => {
        const out = ps.map((p) => ({ ...p }));
        const outById = {}; out.forEach((p) => (outById[p.id] = p));
        const mine = out.filter((p) => p.importId === ingestImportId).sort((a, b) => a.captured - b.captured);

        // stream the next thumbnail
        const next = mine.find((p) => !p.thumbReady);
        if (next) { next.thumbReady = true; next.ingest = 'thumb'; }
        const streamDone = !mine.some((p) => !p.thumbReady);

        const eligible = mine.filter((p) => p.thumbReady && p.flag !== 'reject');
        const eligibleTotal = mine.filter((p) => p.flag !== 'reject').length;

        const stats = {};
        targets.forEach((tg) => {
          const rate = RATE[tg.slots] || 0.3;
          copied.current[tg.id] = Math.min((copied.current[tg.id] || 0) + rate, eligible.length);
          const c = copied.current[tg.id];
          const remaining = eligible.length - c;
          const pct = eligibleTotal ? Math.min(100, (c / eligibleTotal) * 100) : 100;
          const done = streamDone && c >= eligibleTotal - 0.01;
          const active = done ? 0 : Math.max(streamDone ? 0 : 1, Math.min(tg.slots, Math.ceil(remaining + (streamDone ? 0 : tg.slots * 0.4))));
          stats[tg.id] = { active, pct };
        });

        const ref = targets[0];
        if (ref) {
          const c = copied.current[ref.id];
          eligible.forEach((p, i) => {
            if (i < Math.floor(c)) { p.copyPct = 100; p.ingest = 'done'; }
            else if (i < c + ref.slots) { p.copyPct = Math.min(95, Math.max(p.copyPct, (c - i + 1) * 60)); p.ingest = 'copying'; }
            else { p.ingest = 'queued'; }
          });
        }

        const minCopied = Math.min(...targets.map((tg) => copied.current[tg.id] || 0));
        const allDone = streamDone && targets.every((tg) => (copied.current[tg.id] || 0) >= eligibleTotal - 0.01);
        const slowest = targets.reduce((a, b) => (RATE[a.slots] || 1) < (RATE[b.slots] || 1) ? a : b, targets[0]);
        const remSlow = slowest ? eligibleTotal - (copied.current[slowest.id] || 0) : 0;
        const etaSecs = slowest ? (remSlow / (RATE[slowest.slots] || 0.3)) * 0.2 : 0;
        queueMicrotask(() => {
          setIng({ running: !allDone, stats, eta: etaSecs, done: Math.floor(minCopied), total: eligibleTotal });
          if (allDone) setIngestImportId(null);
        });
        return out;
      });
    }, 200);
    return () => clearInterval(id);
  }, [ingestImportId, config]);

  /* ---------- modal actions ---------- */
  const doSave = () => {
    setModal(null);
    setToast(`Metadata written · ${counts.rated} ratings to ${scoped.length * 2} files`);
    setTimeout(() => setToast(''), 2600);
  };
  const doDelete = () => {
    const n = counts.reject;
    const ids = new Set(scoped.filter((p) => p.flag === 'reject').map((p) => p.id));
    setPhotos((ps) => ps.filter((p) => !ids.has(p.id)));
    setModal(null);
    setToast(`Deleted ${n} pair${n !== 1 ? 's' : ''} · ${n * 2} files from ${1 + barTargets.length} locations`);
    setTimeout(() => setToast(''), 2600);
  };

  /* ---------- render ---------- */
  const layout = t.layout;
  const importName = activeImport === 'all' ? 'All Imports' : (activeObj ? activeObj.name : '');
  const center = view === 'clusters'
    ? <ClusterView photos={ordered} selected={selected} focusId={focusId} onSelect={selectPhoto} onLoupe={(id) => { setFocusId(id); setView('loupe'); }} sensitivity={sensitivity} setSensitivity={setSensitivity} />
    : view === 'survey'
      ? <Survey photos={surveyIds.map((id) => byId[id]).filter(Boolean)} focusId={focusId} onFocus={(id) => { setFocusId(id); setSelected(new Set([id])); }}
          setFlag={(id, f) => setFlagOne(id, f)} setRating={(id, r) => setRatingOne(id, r)} onRemove={removeFromSurvey} />
    : view === 'loupe'
      ? <Loupe photo={focus} idx={Math.max(0, list.findIndex((p) => p.id === focusId))} total={list.length}
          onPrev={() => advance(-1)} onNext={() => advance(1)}
          setFlag={(f) => setFlag(focusId, f)} setRating={(r) => setRating(focusId, r)} />
      : <div ref={stageRef} style={{ height: '100%', overflow: 'auto' }}>
          <Grid photos={list} selected={selected} focusId={focusId} onSelect={selectPhoto} onLoupe={(id) => { setFocusId(id); setView('loupe'); }} cell={t.thumb} />
        </div>;

  const inspector = <RightPanel photo={focus} targets={barTargets} selCount={selected.size}
      setFlag={(f) => setFlag(focusId, f)} setRating={(r) => setRating(focusId, r)} />;
  const onLoupe = (id) => { setFocusId(id); setSelected(new Set([id])); setView('loupe'); };

  return (<>
    <div className="app" data-layout={layout}>
      <div className="area-top">
        <TopBar view={view} setView={setView} importName={importName} counts={counts}
          onImport={() => setImporterOpen(true)} surveyCount={surveyIds.length}
          onSave={() => setModal('save')} onDelete={() => setModal('delete')} />
      </div>

      <div className="area-left" style={{ overflowY: 'auto' }}>
        {layout === 'rail'
          ? <IconRail filter={filter} setFilter={setFilter} goClusters={() => setView('clusters')} view={view} onImport={() => setImporterOpen(true)} onCompare={openSurvey} selCount={selected.size} />
          : <>
              <LeftPanel imports={importList} activeImport={activeImport} setActiveImport={setActiveImport}
                renameImport={renameImport} totalCount={photos.length}
                filter={filter} setFilter={setFilter} scoped={scoped}
                goClusters={() => setView('clusters')} clusterCounts={clusterCounts}
                selCount={selected.size} onClearSel={clearSel} onCompare={openSurvey} />
              {layout === 'sidebar' ? inspector : null}
            </>}
      </div>

      <div className="area-center">{center}</div>

      <div className="area-right">
        {layout === 'sidebar'
          ? <Filmstrip photos={list} selected={selected} focusId={focusId} onSelect={selectPhoto} onLoupe={onLoupe} orient="vert" />
          : inspector}
      </div>

      {layout !== 'sidebar' && (
        <div className="area-film">
          <Filmstrip photos={list} selected={selected} focusId={focusId} onSelect={selectPhoto} onLoupe={onLoupe} orient="horiz" />
        </div>
      )}

      <div className="area-ing">
        <IngestBar targets={barTargets} running={ing.running} stats={ing.stats} etaSecs={ing.eta} done={ing.done} total={ing.total} />
      </div>
    </div>

    {importerOpen && (
      <div className="scrim" onMouseDown={() => setImporterOpen(false)}>
        <div onMouseDown={(e) => e.stopPropagation()} style={{ display: 'contents' }}>
          <Launchpad photos={BASE} targets={data.TARGETS} onStart={startImport} onClose={() => setImporterOpen(false)} />
        </div>
      </div>
    )}

    {modal === 'save' && <SaveModal photos={scoped} targets={barTargets} onConfirm={doSave} onCancel={() => setModal(null)} />}
    {modal === 'delete' && <DeleteModal photos={scoped} targets={barTargets} onConfirm={doDelete} onCancel={() => setModal(null)} />}
    <Toast msg={toast} />
    <TweakControls t={t} setTweak={setTweak} />
  </>);
}

function TweakControls({ t, setTweak }) {
  return (
    <TweaksPanel>
      <TweakSection label="Layout arrangement" />
      <TweakRadio label="Panels & filmstrip" value={t.layout}
        options={['classic', 'rail', 'sidebar']}
        onChange={(v) => setTweak('layout', v)} />
      <div style={{ fontSize: 11, color: '#7a7a82', padding: '0 2px 6px', lineHeight: 1.4 }}>
        classic: filters left · inspector right · filmstrip bottom · rail: filters collapse to an icon rail for max image area · sidebar: one wide panel left, vertical filmstrip right
      </div>
      <TweakSection label="Appearance" />
      <TweakColor label="Accent" value={t.accent}
        options={['#3b82f6', '#f59e0b', '#10b981', '#a78bfa']}
        onChange={(v) => setTweak('accent', v)} />
      <TweakSlider label="Thumbnail size" value={t.thumb} min={120} max={260} step={4} unit="px"
        onChange={(v) => setTweak('thumb', v)} />
      <TweakSection label="Culling" />
      <TweakToggle label="Auto-advance after flag" value={t.autoAdvance}
        onChange={(v) => setTweak('autoAdvance', v)} />
    </TweaksPanel>
  );
}

ReactDOM.createRoot(document.getElementById('root')).render(<App />);
