/* panels.jsx — left filter panel, right inspector, filmstrip, ingest bar, rail. */

/* ---------- LEFT PANEL ---------- */
function ImportRow({ imp, active, onSelect, onRename }) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(imp.name);
  const inputRef = useRef(null);
  useEffect(() => { if (editing && inputRef.current) { inputRef.current.focus(); inputRef.current.select(); } }, [editing]);
  const commit = () => { const v = draft.trim(); if (v) onRename(imp.id, v); setEditing(false); };
  return (
    <div className={'imp' + (active ? ' on' : '')} onClick={() => onSelect(imp.id)}
      onDoubleClick={(e) => { e.stopPropagation(); setDraft(imp.name); setEditing(true); }}>
      <Icon name="folder" size={14} style={{ color: active ? 'var(--accent-2)' : 'var(--ink-3)', flex: 'none' }} />
      {editing ? (
        <input ref={inputRef} className="imp-edit mono" value={draft}
          onChange={(e) => setDraft(e.target.value)} onClick={(e) => e.stopPropagation()}
          onBlur={commit} onKeyDown={(e) => { if (e.key === 'Enter') commit(); if (e.key === 'Escape') setEditing(false); }} />
      ) : (
        <span className="imp-nm mono">{imp.name}</span>
      )}
      {!editing && (
        <button className="imp-edit-btn" title="Rename" onClick={(e) => { e.stopPropagation(); setDraft(imp.name); setEditing(true); }}>
          <Icon name="tag" size={12} />
        </button>
      )}
      <span className="ct mono">{imp.n}</span>
    </div>
  );
}

function LeftPanel({ imports, activeImport, setActiveImport, renameImport, totalCount, filter, setFilter, scoped, goClusters, clusterCounts, selCount, onClearSel, onCompare }) {
  const c = {
    all: scoped.length,
    pick: scoped.filter((p) => p.flag === 'accept').length,
    reject: scoped.filter((p) => p.flag === 'reject').length,
    none: scoped.filter((p) => p.flag === 'none').length,
  };
  const FRow = ({ k, label, dot, ct }) => (
    <div className={'frow' + (filter === k ? ' on' : '')} onClick={() => setFilter(k)}>
      {dot ? <span className="dot" style={{ background: dot }} /> : null}
      <span>{label}</span><span className="ct mono">{ct}</span>
    </div>
  );
  return (
    <div className="panel">
      <div className="psec">
        <div className="psec-h">Imports</div>
        <div className="psec-b">
          <div className={'imp all' + (activeImport === 'all' ? ' on' : '')} onClick={() => setActiveImport('all')}>
            <Icon name="layers" size={14} style={{ color: activeImport === 'all' ? 'var(--accent-2)' : 'var(--ink-3)', flex: 'none' }} />
            <span className="imp-nm">All Imports</span>
            <span className="ct mono">{totalCount}</span>
          </div>
          <div className="imp-div" />
          {imports.map((imp) => (
            <ImportRow key={imp.id} imp={imp} active={activeImport === imp.id}
              onSelect={setActiveImport} onRename={renameImport} />
          ))}
        </div>
      </div>

      {selCount > 1 && (
        <div className="psec">
          <div className="psec-b" style={{ paddingTop: 10 }}>
            <div className="selbar">
              <span className="n">{selCount}</span><span>selected</span>
              <span className="sp" />
              <button onClick={onCompare} title="Compare (N)"><Icon name="compare" size={13} /></button>
              <button onClick={onClearSel}>Clear</button>
            </div>
            <div style={{ fontSize: 11, color: 'var(--ink-3)', padding: '0 2px', lineHeight: 1.4 }}>
              P/X flag · 1–5 rate · N compare — applies to all selected
            </div>
          </div>
        </div>
      )}

      <div className="psec">
        <div className="psec-h">Filter</div>
        <div className="psec-b">
          <FRow k="all" label="All photos" ct={c.all} />
          <FRow k="pick" label="Picks" dot="var(--accept)" ct={c.pick} />
          <FRow k="none" label="Unflagged" dot="var(--ink-3)" ct={c.none} />
          <FRow k="reject" label="Rejected" dot="var(--reject)" ct={c.reject} />
          <div style={{ height: 8 }} />
          <StarFilter dir="up" filter={filter} setFilter={setFilter} scoped={scoped} />
          <StarFilter dir="down" filter={filter} setFilter={setFilter} scoped={scoped} />
        </div>
      </div>
    </div>
  );
}

/* clickable star-threshold filter row. dir='up' -> rating >= N; dir='down' -> rating <= N */
function StarFilter({ dir, filter, setFilter, scoped }) {
  const key = dir === 'up' ? 'rup' : 'rdn';
  const active = filter.startsWith(key + ':') ? +filter.split(':')[1] : 0;
  const [hover, setHover] = useState(0);
  const shown = hover || active;
  const stars = (
    <span className="sfstars" onMouseLeave={() => setHover(0)}>
      {[1, 2, 3, 4, 5].map((n) => (
        <svg key={n} viewBox="0 0 24 24" width="13" height="13" fill="currentColor"
          className={'sfstar' + (n <= shown ? ' on' : '')}
          onMouseEnter={() => setHover(n)}
          onClick={(e) => { e.stopPropagation(); setFilter(active === n ? 'all' : key + ':' + n); }}>
          <path d={ICONS.star} />
        </svg>
      ))}
    </span>
  );
  const count = active ? scoped.filter((p) => dir === 'up' ? p.rating >= active : p.rating <= active).length : '';
  return (
    <div className={'frow sfrow' + (active ? ' on' : '')}>
      {dir === 'down' ? <span className="sflbl">down &amp;</span> : null}
      {stars}
      {dir === 'up' ? <span className="sflbl">&amp; up</span> : null}
      <span className="ct mono">{count}</span>
    </div>
  );
}
function Histogram({ seed }) {
  const pts = useMemo(() => {
    let s = seed * 9301 + 49297;
    const rand = () => { s = (s * 9301 + 49297) % 233280; return s / 233280; };
    const N = 48, arr = [];
    const peak = 0.3 + rand() * 0.4;
    for (let i = 0; i < N; i++) {
      const x = i / (N - 1);
      const g = Math.exp(-Math.pow((x - peak) * 3.2, 2)) * (0.7 + rand() * 0.5);
      const g2 = Math.exp(-Math.pow((x - peak * 0.4 - 0.1) * 6, 2)) * 0.4 * rand();
      arr.push(Math.min(1, g + g2));
    }
    return arr;
  }, [seed]);
  const W = 100, H = 100;
  const d = 'M0,100 ' + pts.map((v, i) => `L${(i / (pts.length - 1) * W).toFixed(1)},${(100 - v * 96).toFixed(1)}`).join(' ') + ' L100,100 Z';
  return (
    <svg viewBox="0 0 100 100" preserveAspectRatio="none">
      <path d={d} fill="#ffffff14" stroke="var(--ink-2)" strokeWidth="0.8" vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

function RightPanel({ photo, setFlag, setRating, targets, selCount }) {
  if (!photo) return (
    <div className="panel"><div className="rh" style={{ color: 'var(--ink-3)' }}>No selection</div></div>
  );
  const Row = ({ k, v }) => (<><dt>{k}</dt><dd>{v}</dd></>);
  return (
    <div className="panel">
      <div className="rh">
        <div className="fn">{photo.raw.replace('.CR2', '')}<span className="ext">.CR2</span></div>
        <div className="pair">
          <span className="pairtag"><span className="b" />RAW · {photo.rawMB} MB</span>
          <span className="pairtag jpg"><span className="b" />JPG · {photo.jpgMB} MB</span>
        </div>
        <div className="sub" style={{ color: 'var(--ink-3)', fontSize: 11, marginTop: 6 }}>
          Captured {fmtTime(photo.captured)} · {photo.clusterName}
        </div>
      </div>

      <div className="psec" style={{ padding: '12px 13px' }}>
        <div className="psec-h" style={{ padding: '0 0 9px' }}>Flag &amp; rating
          {selCount > 1 ? <span className="mono" style={{ color: 'var(--accent-2)', fontWeight: 700 }}>{selCount} selected</span> : null}</div>
        <div className="flags">
          <button className={'flagbtn' + (photo.flag === 'accept' ? ' on-accept' : '')} onClick={() => setFlag('accept')}>
            <Icon name="check" size={17} /> Pick <span className="kbd">P</span>
          </button>
          <button className={'flagbtn' + (photo.flag === 'reject' ? ' on-reject' : '')} onClick={() => setFlag('reject')}>
            <Icon name="x" size={17} /> Reject <span className="kbd">X</span>
          </button>
        </div>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginTop: 13 }}>
          <Stars value={photo.rating} onSet={setRating} />
          <span className="mono" style={{ color: 'var(--ink-3)', fontSize: 11 }}>{photo.rating || '–'}/5</span>
        </div>
      </div>

      <div className="psec">
        <div className="psec-h">Histogram</div>
        <div className="histo"><Histogram seed={photo.id + 1} /></div>
      </div>

      <div className="psec">
        <div className="psec-h">Capture</div>
        <dl className="exif">
          <Row k="Camera" v="EOS R6 II" />
          <Row k="Lens" v={photo.lens} />
          <Row k="Focal" v={photo.focal + ' mm'} />
          <Row k="Aperture" v={photo.aperture} />
          <Row k="Shutter" v={photo.shutter + 's'} />
          <Row k="ISO" v={photo.iso} />
          <Row k="Pixels" v={photo.dims} />
        </dl>
      </div>

      <div className="psec">
        <div className="psec-h">Replication</div>
        <div className="copylist">
          {targets.map((t, i) => {
            const pct = photo.flag === 'reject' ? Math.min(photo.copyPct, 30)
              : photo.copyPct;
            const done = pct >= 100;
            return (
              <div className="copyitem" key={t.id}>
                <div className="top"><span>{t.name}</span>
                  <span className="mono">{photo.flag === 'reject' && !done ? 'held' : done ? 'done' : Math.round(pct) + '%'}</span></div>
                <div className="bar"><i className={done ? 'done' : ''} style={{ width: pct + '%' }} /></div>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

/* ---------- FILMSTRIP ---------- */
function Filmstrip({ photos, selected, focusId, onSelect, onLoupe, orient }) {
  const ref = useRef(null);
  useEffect(() => {
    const el = ref.current; if (!el) return;
    const sel = el.querySelector('.fcell.focus');
    if (sel) {
      const r = sel.getBoundingClientRect(), pr = el.getBoundingClientRect();
      if (orient === 'vert') { if (r.top < pr.top || r.bottom > pr.bottom) el.scrollTop += r.top - pr.top - 40; }
      else { if (r.left < pr.left || r.right > pr.right) el.scrollLeft += r.left - pr.left - 60; }
    }
  }, [focusId, orient]);
  return (
    <div className="film" data-orient={orient}>
      <div className="film-h">
        <Icon name="grid" size={13} /> Filmstrip
        <span className="mono" style={{ color: 'var(--ink-2)' }}>{photos.length}</span>
        <span className="spacer" style={{ flex: 1 }} />
        {selected.size > 1 ? <span className="mono" style={{ color: 'var(--accent-2)' }}>{selected.size} selected</span> : <span style={{ color: 'var(--ink-3)' }}>by capture time</span>}
      </div>
      <div className="film-track" ref={ref}>
        {photos.map((p) => (
          <div key={p.id} className={'fcell' + (selected.has(p.id) ? ' sel' : '') + (p.id === focusId ? ' focus' : '') + (p.flag === 'accept' ? ' accept' : '') + (p.flag === 'reject' ? ' reject' : '')}
            onMouseDown={(e) => onSelect(p.id, { ctrl: e.ctrlKey, meta: e.metaKey, shift: e.shiftKey })} onDoubleClick={() => onLoupe(p.id)}>
            {p.thumbReady ? <img src={p.file} alt="" draggable="false" /> : <div className="ph" />}
            {p.flag === 'accept' ? <span className="fchip" style={{ background: 'var(--accept)', color: '#06251a' }}><Icon name="check" size={9} /></span> : null}
            {p.flag === 'reject' ? <span className="fchip" style={{ background: 'var(--reject)', color: '#2a0a0f' }}><Icon name="x" size={9} /></span> : null}
            <div className="mini"><MiniStars value={p.rating} /></div>
          </div>
        ))}
      </div>
    </div>
  );
}

/* ---------- INGEST BAR ---------- */
function IngestBar({ targets, running, stats, etaSecs, done, total }) {
  return (
    <div className="ingest">
      <div className="ing-title">
        <span className={'pulse' + (running ? '' : ' idle')} />
        {running ? 'Ingesting' : 'Idle'}
        {running ? <span className="mono" style={{ color: 'var(--ink-3)', marginLeft: 4 }}>{done}/{total}</span> : null}
      </div>
      <div className="ing-targets">
        {targets.map((t) => {
          const s = stats[t.id] || { active: 0, pct: running ? 0 : 100 };
          return (
            <div className="ing-t" key={t.id}>
              <Icon name={t.kind === 'local' ? 'ssd' : t.kind === 'network' ? 'nas' : 'cloud'} size={14} style={{ color: 'var(--ink-3)' }} />
              <span className="nm">{t.name.split(' ')[0]} {t.name.includes('Drive') ? 'Drive' : ''}</span>
              <span className="slotdots">
                {Array.from({ length: Math.min(t.slots, 16) }).map((_, i) => (
                  <i key={i} className={i < s.active ? 'act' : ''} />
                ))}
              </span>
              <span className="slots">{s.active}/{t.slots}</span>
              <div className="track"><i className={t.kind} style={{ width: s.pct + '%' }} /></div>
            </div>
          );
        })}
      </div>
      <div className="ing-eta">{running ? 'ETA ' + fmtClock(etaSecs) : '—'}</div>
    </div>
  );
}

/* ---------- ICON RAIL (rail layout) ---------- */
function IconRail({ filter, setFilter, goClusters, view, onImport, onCompare, selCount }) {
  const items = [
    { k: 'all', icon: 'grid', t: 'All' },
    { k: 'pick', icon: 'check', t: 'Picks' },
    { k: 'reject', icon: 'x', t: 'Rejected' },
    { k: 'r3', icon: 'star', t: 'Rated' },
  ];
  return (
    <div className="rail">
      <button className="rail-import" title="Import" onClick={onImport}><Icon name="plus" size={18} /></button>
      <div className="div" />
      {items.map((it) => (
        <button key={it.k} className={filter === it.k ? 'on' : ''} title={it.t} onClick={() => setFilter(it.k)}>
          <Icon name={it.icon} size={18} />
        </button>
      ))}
      <div className="div" />
      <button className={view === 'clusters' ? 'on' : ''} title="Clusters (L)" onClick={goClusters}><Icon name="layers" size={18} /></button>
      <button className={view === 'survey' ? 'on' : ''} title="Compare (N)" onClick={onCompare} style={selCount > 1 ? { color: 'var(--accent-2)' } : null}><Icon name="compare" size={18} /></button>
      <div className="div" />
      <button title="Settings"><Icon name="settings" size={18} /></button>
    </div>
  );
}

Object.assign(window, { LeftPanel, RightPanel, Filmstrip, IngestBar, IconRail });
