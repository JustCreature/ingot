/* clusters.jsx — sensitivity-driven grouping view. */
function ClusterView({ photos, selected, focusId, onSelect, onLoupe, sensitivity, setSensitivity }) {
  const groups = useMemo(() => computeClusters(photos, sensitivity), [photos, sensitivity]);
  const byId = useMemo(() => { const m = {}; photos.forEach((p) => (m[p.id] = p)); return m; }, [photos]);

  return (
    <div className="clview">
      <div className="cl-toolbar">
        <div className="lab"><Icon name="layers" size={15} /> <b style={{ color: 'var(--ink)' }}>{groups.length}</b> groups
          <span style={{ color: 'var(--ink-3)' }}>from {photos.length} photos</span></div>
        <div style={{ display: 'flex', flexDirection: 'column' }}>
          <div className="lab"><Icon name="sliders" size={14} /> Sensitivity
            <input className="rng" type="range" min="0" max="100" value={sensitivity}
              onChange={(e) => setSensitivity(+e.target.value)} />
            <span className="mono" style={{ color: 'var(--accent-2)' }}>{sensitivity}</span>
          </div>
          <div className="sens-marks" style={{ marginLeft: 110 }}>
            <span>scene only</span><span>+ subject position</span>
          </div>
        </div>
        <span className="spacer" style={{ flex: 1 }} />
        <span style={{ color: 'var(--ink-3)', fontSize: 11, maxWidth: 280 }}>
          Low groups by averaged scene color; raise it to split on where the subject stands within the frame.
        </span>
      </div>

      {groups.map((g) => {
        const first = byId[g.photoIds[0]];
        const color = first ? CLUSTER_COLOR[first.cluster] : 'var(--accent)';
        return (
          <div className="cluster" key={g.id}>
            <div className="cluster-h">
              <span className="sw" style={{ background: color }} />
              <span className="nm">{g.clusterName}</span>
              <span className="ct mono">{g.photoIds.length} frames</span>
              <span className="line" />
              <span className="ct mono">{g.photoIds.filter((id) => byId[id].flag === 'accept').length} picks</span>
            </div>
            <div className="gridwrap">
              {g.photoIds.map((id) => {
                const p = byId[id];
                return <Cell key={id} p={p} selected={selected.has(id)} focused={id === focusId} onSelect={onSelect} onLoupe={onLoupe} />;
              })}
            </div>
          </div>
        );
      })}
    </div>
  );
}
window.ClusterView = ClusterView;
