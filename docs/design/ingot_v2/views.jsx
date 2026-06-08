/* views.jsx — top bar, grid workspace, loupe. */

function TopBar({ view, setView, importName, counts, onImport, onSave, onDelete, surveyCount }) {
  return (
    <div className="topbar">
      <div className="brand">
        <div className="brand-mark"><Icon name="bolt" size={12} fill="#fff" /></div>
        <div className="brand-name">IN<b>GOT</b></div>
      </div>
      <button className="btn primary" onClick={onImport}><Icon name="plus" size={14} /> Import</button>
      <div className="seg">
        <button className={view === 'grid' ? 'on' : ''} onClick={() => setView('grid')}><Icon name="grid" size={14} /> Grid <span className="kbd">G</span></button>
        <button className={view === 'loupe' ? 'on' : ''} onClick={() => setView('loupe')}><Icon name="loupe" size={14} /> Loupe <span className="kbd">E</span></button>
        <button className={view === 'clusters' ? 'on' : ''} onClick={() => setView('clusters')}><Icon name="layers" size={14} /> Clusters <span className="kbd">L</span></button>
      </div>
      {view === 'survey' && (
        <div className="survey-pill">
          <Icon name="compare" size={14} /> Comparing <b>{surveyCount}</b>
          <button title="Exit compare" onClick={() => setView('grid')}><Icon name="x" size={12} /></button>
        </div>
      )}
      <div className="crumb">
        <Icon name="folder" size={14} />
        <span className="mono">{importName}</span>
      </div>
      <div className="spacer" />
      <div className="crumb" style={{ marginRight: 4 }}>
        <span style={{ color: 'var(--accept)' }}>● {counts.pick}</span>
        <span style={{ color: 'var(--reject)' }}>● {counts.reject}</span>
        <span className="mono" style={{ color: 'var(--ink-3)' }}>{counts.rated}★</span>
      </div>
      <button className="btn" onClick={onSave}>
        <Icon name="tag" size={14} /> Save metadata
      </button>
      <button className="btn danger" onClick={onDelete} disabled={counts.reject === 0}>
        <Icon name="trash" size={14} /> Delete rejected
        <span className="kbd" style={{ borderColor: '#4a2026' }}>{counts.reject}</span>
      </button>
    </div>
  );
}

function Cell({ p, selected, focused, onSelect, onLoupe }) {
  return (
    <div className={'cell' + (selected ? ' sel' : '') + (focused ? ' focus' : '') + (p.flag === 'accept' ? ' accept' : '') + (p.flag === 'reject' ? ' reject' : '')}
      onMouseDown={(e) => onSelect(p.id, { ctrl: e.ctrlKey, meta: e.metaKey, shift: e.shiftKey })} onDoubleClick={() => onLoupe(p.id)}>
      {p.thumbReady ? <img src={p.file} alt="" draggable="false" /> : <div className="ph" />}
      {p.thumbReady && (
        <>
          <div className="ov top">
            <span className="pairbadge"><span className="b" />RAW<span style={{ opacity: .55 }}>+JPG</span></span>
            {p.flag === 'accept' ? <span className="flagchip accept"><Icon name="check" size={11} /></span> : null}
            {p.flag === 'reject' ? <span className="flagchip reject"><Icon name="x" size={11} /></span> : null}
          </div>
          <div className="ov bot">
            <span className="fnm">{p.raw.replace('.CR2', '')}</span>
            <MiniStars value={p.rating} />
          </div>
        </>
      )}
    </div>
  );
}

function Grid({ photos, selected, focusId, onSelect, onLoupe, cell }) {
  return (
    <div className="stage">
      <div className="gridwrap" style={{ '--cell': cell + 'px' }}>
        {photos.map((p) => (
          <Cell key={p.id} p={p} selected={selected.has(p.id)} focused={p.id === focusId} onSelect={onSelect} onLoupe={onLoupe} />
        ))}
        {photos.length === 0 && (
          <div style={{ color: 'var(--ink-3)', padding: 40, gridColumn: '1/-1', textAlign: 'center' }}>
            No photos match this filter.
          </div>
        )}
      </div>
    </div>
  );
}

function Survey({ photos, focusId, onFocus, setFlag, setRating, onRemove }) {
  const n = photos.length;
  const cols = Math.max(1, Math.ceil(Math.sqrt(n)));
  if (!n) return <div className="survey-empty">Select photos in the grid (⌘/Ctrl or Shift-click), then press <span className="kbd">N</span> to compare.</div>;
  return (
    <div className="survey" style={{ gridTemplateColumns: `repeat(${cols}, 1fr)` }}>
      {photos.map((p) => (
        <div key={p.id} className={'sv-cell' + (p.id === focusId ? ' focus' : '') + (p.flag === 'reject' ? ' reject' : '')}
          onMouseDown={() => onFocus(p.id)}>
          <div className="sv-imgwrap">
            {p.thumbReady ? <img src={p.file} alt="" draggable="false" /> : <div className="ph" />}
            {p.flag === 'accept' ? <span className="flagchip accept sv-flag"><Icon name="check" size={12} /></span> : null}
            {p.flag === 'reject' ? <span className="flagchip reject sv-flag"><Icon name="x" size={12} /></span> : null}
            <button className="sv-remove" title="Remove from compare" onMouseDown={(e) => { e.stopPropagation(); onRemove(p.id); }}><Icon name="x" size={12} /></button>
          </div>
          <div className="sv-bar">
            <span className="fn mono">{p.raw.replace('.CR2', '')}</span>
            <div className="sv-acts">
              <button className={'sv-fb' + (p.flag === 'reject' ? ' on-reject' : '')} onMouseDown={(e) => { e.stopPropagation(); setFlag(p.id, 'reject'); }}><Icon name="x" size={13} /></button>
              <button className={'sv-fb' + (p.flag === 'accept' ? ' on-accept' : '')} onMouseDown={(e) => { e.stopPropagation(); setFlag(p.id, 'accept'); }}><Icon name="check" size={13} /></button>
              <span className="sv-stars" onMouseDown={(e) => e.stopPropagation()}><Stars value={p.rating} onSet={(r) => setRating(p.id, r)} size={17} /></span>
            </div>
          </div>
        </div>
      ))}
    </div>
  );
}

function Loupe({ photo, idx, total, onPrev, onNext, setFlag, setRating }) {
  if (!photo) return <div className="loupe" />;
  const stateLabel = {
    pending: ['var(--ink-3)', 'reading card'], thumb: ['var(--accent-2)', 'preview ready'],
    queued: ['var(--accent-2)', 'queued'], copying: ['var(--accent-2)', 'copying'],
    done: ['var(--accept)', 'copied'], skipped: ['var(--ink-3)', 'held'],
  }[photo.ingest] || ['var(--ink-3)', photo.ingest];
  return (
    <div className={'loupe' + (photo.flag === 'reject' ? ' reject' : '')}>
      {photo.thumbReady ? <img className="loupe-img" src={photo.file} alt="" draggable="false" /> : <div className="ph" style={{ width: '60%', height: '60%', borderRadius: 8 }} />}

      <button className="loupe-nav prev" onClick={onPrev}><Icon name="chevL" size={20} /></button>
      <button className="loupe-nav next" onClick={onNext}><Icon name="chevR" size={20} /></button>

      <div className="loupe-hud">
        <span className="fn">{photo.raw}</span>
        <span className="pairtag"><span className="b" />+JPG</span>
        <span className="mono" style={{ color: 'var(--ink-3)' }}>{idx + 1}/{total}</span>
      </div>

      <div className="loupe-flag">
        <button className={'flagbtn' + (photo.flag === 'reject' ? ' on-reject' : '')} style={{ width: 90 }} onClick={() => setFlag('reject')}>
          <Icon name="x" size={16} /> Reject <span className="kbd">X</span>
        </button>
        <button className={'flagbtn' + (photo.flag === 'accept' ? ' on-accept' : '')} style={{ width: 90 }} onClick={() => setFlag('accept')}>
          <Icon name="check" size={16} /> Pick <span className="kbd">P</span>
        </button>
      </div>

      <div className="loupe-rate">
        <Stars value={photo.rating} onSet={setRating} size={24} />
        <span className="mono" style={{ color: 'var(--ink-3)', fontSize: 11 }}>press 1–5</span>
      </div>

      <div className="loupe-state">
        <span className="state-pill"><span className="b" style={{ background: stateLabel[0] }} />{stateLabel[1]}</span>
      </div>
    </div>
  );
}

Object.assign(window, { TopBar, Grid, Survey, Loupe });
