/* modals.jsx — save metadata, delete rejected, toast. */
function Modal({ children }) {
  return <div className="scrim">{<div className="modal">{children}</div>}</div>;
}

function SaveModal({ photos, targets, onConfirm, onCancel }) {
  const rated = photos.filter((p) => p.rating > 0).length;
  const flagged = photos.filter((p) => p.flag !== 'none').length;
  return (
    <div className="scrim" onClick={onCancel}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-h">
          <div className="mi" style={{ background: 'var(--accent-dim)', color: 'var(--accent-2)' }}><Icon name="tag" size={18} /></div>
          <div><h3>Save metadata</h3>
            <div style={{ color: 'var(--ink-3)', fontSize: 12 }}>Ratings &amp; flags → sidecars + EXIF</div></div>
        </div>
        <div className="modal-b">
          <p style={{ marginTop: 0 }}>Writes a <span className="mono" style={{ color: 'var(--accent-2)' }}>.xmp</span> sidecar
            next to each <span className="mono">.CR2</span> and embeds the matching rating into the paired
            <span className="mono"> .JPG</span> — mirrored across all targets.</p>
          <div className="bigstat">
            <div><div className="n">{rated}</div><div className="l">ratings</div></div>
            <div><div className="n">{flagged}</div><div className="l">flags</div></div>
            <div><div className="n">{photos.length * 2}</div><div className="l">files touched</div></div>
            <div><div className="n">{targets.length}</div><div className="l">targets</div></div>
          </div>
        </div>
        <div className="modal-f">
          <button className="btn ghost" onClick={onCancel}>Cancel</button>
          <button className="btn primary" onClick={onConfirm}><Icon name="check" size={14} /> Write metadata</button>
        </div>
      </div>
    </div>
  );
}

function DeleteModal({ photos, targets, onConfirm, onCancel }) {
  const rej = photos.filter((p) => p.flag === 'reject');
  return (
    <div className="scrim" onClick={onCancel}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-h">
          <div className="mi" style={{ background: '#2a1419', color: 'var(--reject)' }}><Icon name="trash" size={18} /></div>
          <div><h3>Delete rejected photos</h3>
            <div style={{ color: 'var(--ink-3)', fontSize: 12 }}>RAW + JPEG, every location</div></div>
        </div>
        <div className="modal-b">
          <div className="bigstat">
            <div><div className="n">{rej.length}</div><div className="l">rejected pairs</div></div>
            <div><div className="n" style={{ color: 'var(--reject)' }}>{rej.length * 2}</div><div className="l">files erased</div></div>
            <div><div className="n">{1 + targets.length}</div><div className="l">locations</div></div>
          </div>
          <div className="warn-note">
            <Icon name="info" size={16} style={{ flex: 'none', marginTop: 1 }} />
            <span>Each rejected <span className="mono">.CR2</span> and its <span className="mono">.JPG</span> twin are erased
              from the SD card <i>and</i> {targets.map((t) => t.name).join(', ')}. This cannot be undone.</span>
          </div>
        </div>
        <div className="modal-f">
          <button className="btn ghost" onClick={onCancel}>Keep them</button>
          <button className="btn danger" onClick={onConfirm} disabled={rej.length === 0}>
            <Icon name="trash" size={14} /> Delete {rej.length} pair{rej.length !== 1 ? 's' : ''}
          </button>
        </div>
      </div>
    </div>
  );
}

function Toast({ msg }) {
  if (!msg) return null;
  return <div className="toast"><Icon name="check" size={15} /> {msg}</div>;
}

Object.assign(window, { SaveModal, DeleteModal, Toast });
