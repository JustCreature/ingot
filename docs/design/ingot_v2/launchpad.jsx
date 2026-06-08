/* launchpad.jsx — ingest settings (rendered as an overlay from "+ Import"). */
function Launchpad({ photos, targets, onStart, onClose }) {
  const [tg, setTg] = useState({ ssd: true, nas: true, gdrive: false });
  const [hier, setHier] = useState('YYYY/YYYY-MM-DD');
  const [startBg, setStartBg] = useState(true);
  const [skipRej, setSkipRej] = useState(true);

  const date = new Date(2026, 4, 12);
  const Y = '2026';
  const pretty = useMemo(() => {
    const yyyy = '2026', mm = '05', dd = '12';
    switch (hier) {
      case 'YYYY/YYYY-MM-DD': return [['seg-y', yyyy], ['/', '/'], ['seg-d', `${yyyy}-${mm}-${dd}`]];
      case 'YYYY/MM-DD-YYYY': return [['seg-y', yyyy], ['/', '/'], ['seg-d', `${mm}-${dd}-${yyyy}`]];
      case 'YYYY/MM/DD':      return [['seg-y', yyyy], ['/', '/'], ['seg-m', mm], ['/', '/'], ['seg-d', dd]];
      case 'flat':            return [['seg-d', `${yyyy}-${mm}-${dd}_import`]];
      default: return [['seg-y', yyyy]];
    }
  }, [hier]);

  const selTargets = targets.filter((t) => tg[t.id]);
  const pairs = photos.length;
  const gb = (photos.reduce((s, p) => s + p.rawMB + p.jpgMB, 0) / 1024).toFixed(1);

  const tIcon = { local: 'ssd', network: 'nas', cloud: 'cloud' };

  return (
    <div className="lp overlay">
      <div className="lp-top">
        <div className="brand">
          <div className="brand-mark"><Icon name="bolt" size={13} fill="#fff" /></div>
          <div className="brand-name">IN<b>GOT</b></div>
        </div>
        <div className="crumb"><Icon name="scan" size={14} /><span>New import</span></div>
        <div className="spacer" />
        <button className="btn ghost" onClick={onClose}><Icon name="x" size={15} /> Close</button>
      </div>

      <div className="lp-body">
        <div className="lp-card">
          <div className="lp-hero">
            <h1>New import</h1>
            <p>Pull RAW + JPEG pairs off the card, triage them as they stream in, and replicate
              to every target in the background. Nothing is moved or deleted until you say so.</p>
          </div>

          <div className="lp-cols">
            {/* SOURCE */}
            <div className="lp-block">
              <h3><Icon name="sd" size={15} /> Source</h3>
              <p className="hint">Card detected automatically.</p>
              <div className="dev" style={{ borderColor: 'var(--accent-dim)', background: '#11203a55' }}>
                <div className="ic" style={{ color: 'var(--accent-2)' }}><Icon name="sd" size={17} /></div>
                <div className="meta">
                  <div className="nm">EOS_DIGITAL · SD</div>
                  <div className="sub mono">{pairs} pairs · {gb} GB · CR2 + JPG</div>
                </div>
                <div className="state-pill" style={{ background: 'none', border: '1px solid var(--line)' }}>
                  <span className="b" style={{ background: 'var(--accept)' }} />ready
                </div>
              </div>
              <div className="field">
                <label>Pairing</label>
                <div className="dev" style={{ margin: 0 }}>
                  <div className="ic"><Icon name="layers" size={16} /></div>
                  <div className="meta">
                    <div className="nm" style={{ fontSize: 12 }}>Group CR2 + JPG by name</div>
                    <div className="sub">JPG follows its RAW for every action</div>
                  </div>
                  <div className="tg on"><i /></div>
                </div>
              </div>
            </div>

            <div className="lp-arrow"><Icon name="arrowR" size={18} /></div>

            {/* TARGETS */}
            <div className="lp-block">
              <h3><Icon name="folder" size={15} /> Destinations</h3>
              <p className="hint">Copied concurrently, throttled per target.</p>
              {targets.map((t) => (
                <button key={t.id} className="dev" style={{ width: '100%', textAlign: 'left',
                    borderColor: tg[t.id] ? 'var(--accent-dim)' : 'var(--line)',
                    background: tg[t.id] ? '#11203a55' : 'var(--panel-2)' }}
                  onClick={() => setTg((s) => ({ ...s, [t.id]: !s[t.id] }))}>
                  <div className="ic" style={{ color: tg[t.id] ? 'var(--accent-2)' : 'var(--ink-3)' }}>
                    <Icon name={tIcon[t.kind]} size={16} />
                  </div>
                  <div className="meta">
                    <div className="nm">{t.name}</div>
                    <div className="sub mono">{t.path} · {t.slots} slots</div>
                  </div>
                  <div className={'tg' + (tg[t.id] ? ' on' : '')}><i /></div>
                </button>
              ))}
              <button className="lp-add"><Icon name="plus" size={14} /> Add destination…</button>

              <div className="field">
                <label>Folder hierarchy</label>
                <div className="select">
                  <select value={hier} onChange={(e) => setHier(e.target.value)}>
                    <option value="YYYY/YYYY-MM-DD">YYYY / YYYY-MM-DD</option>
                    <option value="YYYY/MM-DD-YYYY">YYYY / MM-DD-YYYY</option>
                    <option value="YYYY/MM/DD">YYYY / MM / DD</option>
                    <option value="flat">Single dated folder</option>
                  </select>
                </div>
                <div className="preview-path">
                  <span style={{ color: 'var(--ink-3)' }}>…/2026/ </span>
                  {pretty.map(([c, v], i) => <span key={i} className={c}>{v}</span>)}
                  <span style={{ color: 'var(--ink-3)' }}> / IMG_0421.CR2</span>
                </div>
              </div>
            </div>
          </div>

          <div style={{ display: 'flex', gap: 26, marginTop: 16, padding: '0 2px', flexWrap: 'wrap' }}>
            <label className="toggle-row" onClick={() => setStartBg((v) => !v)}>
              <div className={'tg' + (startBg ? ' on' : '')}><i /></div>
              Start copying in the background while I cull
            </label>
            <label className="toggle-row" onClick={() => setSkipRej((v) => !v)}>
              <div className={'tg' + (skipRej ? ' on' : '')}><i /></div>
              Deprioritize the copy queue for rejected frames
            </label>
          </div>
        </div>
      </div>

      <div className="lp-foot">
        <div className="lp-stat"><b>{pairs}</b> pairs · <b>{gb} GB</b></div>
        <div className="lp-stat" style={{ color: 'var(--ink-3)' }}>→ {selTargets.length} destination{selTargets.length !== 1 ? 's' : ''}</div>
        <div className="spacer" />
        <button className="btn primary" style={{ padding: '9px 18px', fontSize: 13 }}
          disabled={selTargets.length === 0}
          onClick={() => onStart({ targets: selTargets, hier, startBg, skipRej })}>
          <Icon name="scan" size={16} /> Scan &amp; Ingest
          <span className="kbd" style={{ color: '#06122b', borderColor: '#1f4fa0', background: '#ffffff33' }}>↵</span>
        </button>
      </div>
    </div>
  );
}
window.Launchpad = Launchpad;
