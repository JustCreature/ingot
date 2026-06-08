/* Ingot — data model. Each item is a RAW(CR2)+JPEG pair treated as one asset. */
(function () {
  const CLUSTERS = [
    { key: 'wall',   name: 'Old town wall',  n: 8, hue: 30  },
    { key: 'tower',  name: 'Glass tower',    n: 7, hue: 210 },
    { key: 'oak',    name: 'The big oak',    n: 7, hue: 110 },
    { key: 'harbor', name: 'Harbor sunset',  n: 6, hue: 20  },
    { key: 'cafe',   name: 'Cafe interior',  n: 6, hue: 32  },
    { key: 'street', name: 'Street candids', n: 6, hue: 215 },
  ];

  // tiny deterministic PRNG so the mock is stable across reloads
  function rng(seed) { let s = seed; return () => (s = (s * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff; }

  const LENSES = ['RF 24-70mm F2.8', 'RF 50mm F1.2', 'RF 35mm F1.8', 'RF 70-200mm F2.8'];
  const SHUTTERS = ['1/2000', '1/1000', '1/500', '1/250', '1/125', '1/60'];
  const APERTURES = ['f/1.4', 'f/1.8', 'f/2.0', 'f/2.8', 'f/4.0', 'f/5.6'];
  const ISOS = [100, 100, 200, 200, 400, 800, 1600];

  let id = 0;
  const PHOTOS = [];
  // capture session starts 2026-05-12 14:02
  let t = new Date(2026, 4, 12, 14, 2, 0).getTime();

  CLUSTERS.forEach((c) => {
    const r = rng(c.hue + 7);
    for (let i = 0; i < c.n; i++) {
      const seq = i;
      t += Math.round(2000 + r() * 9000); // 2–11s between frames
      const base = String(c.key) + '_' + String(i + 1).padStart(2, '0');
      // intra-cluster feature drift (simulates subject moving / recomposing)
      const pos = (i / c.n) * 2 - 1;             // -1..1 horizontal position of subject
      const hue = c.hue + (r() - 0.5) * 10;       // small color drift
      PHOTOS.push({
        id: id++,
        cluster: c.key,
        clusterName: c.name,
        file: (window.__resources && window.__resources[base]) || ('images/' + base + '.png'),
        raw: base.toUpperCase() + '.CR2',
        jpg: base.toUpperCase() + '.JPG',
        captured: t,
        seq,
        pos, hue,
        flag: 'none',          // none | accept | reject
        rating: 0,             // 0..5
        // ingest sim
        ingest: 'pending',     // pending | thumb | queued | copying | done | skipped
        thumbReady: false,
        copyPct: 0,
        // exif
        lens: LENSES[Math.floor(r() * LENSES.length)],
        shutter: SHUTTERS[Math.floor(r() * SHUTTERS.length)],
        aperture: APERTURES[Math.floor(r() * APERTURES.length)],
        iso: ISOS[Math.floor(r() * ISOS.length)],
        focal: [24, 35, 50, 70, 85, 135][Math.floor(r() * 6)],
        rawMB: +(22 + r() * 9).toFixed(1),
        jpgMB: +(6 + r() * 4).toFixed(1),
        dims: '6720 × 4480',
      });
    }
  });

  // Targets for background ingest
  const TARGETS = [
    { id: 'ssd',    name: 'Local NVMe SSD',     path: '/Volumes/Photos/2026', slots: 16, kind: 'local'  },
    { id: 'nas',    name: 'Studio NAS',          path: 'smb://nas/raw/2026',   slots: 8,  kind: 'network'},
    { id: 'gdrive', name: 'Google Drive (sync)', path: '~/GDrive/Ingest/2026', slots: 4,  kind: 'cloud'  },
  ];

  window.INGOT_DATA = { CLUSTERS, PHOTOS, TARGETS };

  /* clustering: greedy split within named clusters; threshold shrinks as
     sensitivity rises, so more (tighter) groups emerge. Returns array of
     { id, clusterName, photoIds[] }. */
  window.computeClusters = function (photos, sensitivity /*0..100*/) {
    const groups = [];
    const byCluster = {};
    photos.forEach((p) => { (byCluster[p.cluster] = byCluster[p.cluster] || []).push(p); });
    // at sensitivity 0 -> one group per named cluster.
    // as it rises, also split on position drift (where the subject stands).
    const posThresh = 2.2 - (sensitivity / 100) * 2.0; // 2.2 (never splits) -> 0.2 (splits a lot)
    Object.keys(byCluster).forEach((k) => {
      const list = byCluster[k].slice().sort((a, b) => a.seq - b.seq);
      let cur = null, anchor = null;
      list.forEach((p) => {
        if (!cur || Math.abs(p.pos - anchor.pos) > posThresh) {
          cur = { id: groups.length, clusterName: p.clusterName, photoIds: [] };
          groups.push(cur);
          anchor = p;
        }
        cur.photoIds.push(p.id);
      });
    });
    return groups;
  };
})();
