/* ui.jsx — icons + small shared atoms. Exported to window. */
const { useState, useEffect, useRef, useMemo, useCallback } = React;

const ICONS = {
  sd:      'M4 4h11l5 5v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1zM9 4v3M12 4v3M15 4v3',
  ssd:     'M3 7a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2zM7 15h.01M11 15h2',
  nas:     'M4 5h16v6H4zM4 13h16v6H4zM7 8h.01M7 16h.01M16 8h3M16 16h3',
  cloud:   'M17.5 19a4.5 4.5 0 0 0 .5-8.97A6 6 0 0 0 6.34 9.5 4 4 0 0 0 7 19z',
  grid:    'M3 3h7v7H3zM14 3h7v7h-7zM14 14h7v7h-7zM3 14h7v7H3z',
  loupe:   'M3 9V5a2 2 0 0 1 2-2h4M21 9V5a2 2 0 0 0-2-2h-4M3 15v4a2 2 0 0 0 2 2h4M21 15v4a2 2 0 0 1-2 2h-4',
  layers:  'M12 2 2 7l10 5 10-5zM2 12l10 5 10-5M2 17l10 5 10-5',
  check:   'M20 6 9 17l-5-5',
  x:       'M18 6 6 18M6 6l12 12',
  star:    'M12 2.5l2.9 6 6.6.9-4.8 4.6 1.2 6.5L12 18.4 6.1 20.5l1.2-6.5L2.5 9.4l6.6-.9z',
  chevL:   'M15 18l-6-6 6-6',
  chevR:   'M9 18l6-6-6-6',
  chevD:   'M6 9l6 6 6-6',
  folder:  'M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z',
  trash:   'M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2M6 6l1 14a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1l1-14',
  tag:     'M20.6 13.4 12 22l-9-9V4a1 1 0 0 1 1-1h9zM7.5 7.5h.01',
  scan:    'M3 7V5a2 2 0 0 1 2-2h2M17 3h2a2 2 0 0 1 2 2v2M21 17v2a2 2 0 0 1-2 2h-2M7 21H5a2 2 0 0 1-2-2v-2M3 12h18',
  play:    'M6 4l14 8-14 8z',
  info:    'M12 16v-5M12 8h.01M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20z',
  sliders: 'M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6',
  plus:    'M12 5v14M5 12h14',
  arrowR:  'M5 12h14M13 6l6 6-6 6',
  flag:    'M4 22V4M4 4h13l-2 4 2 4H4',
  settings:'M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM19.4 15a1.6 1.6 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.6 1.6 0 0 0-2.7 1.1V21a2 2 0 1 1-4 0v-.1A1.6 1.6 0 0 0 7 19.4a1.6 1.6 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.6 1.6 0 0 0-1.1-2.7H1a2 2 0 1 1 0-4h.1A1.6 1.6 0 0 0 2.6 7a1.6 1.6 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.6 1.6 0 0 0 1.8.3H7a1.6 1.6 0 0 0 1-1.5V1a2 2 0 1 1 4 0v.1a1.6 1.6 0 0 0 2.7 1.1l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.6 1.6 0 0 0-.3 1.8V7a1.6 1.6 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.6 1.6 0 0 0-1.5 1z',
  eye:     'M1 12s4-7 11-7 11 7 11 7-4 7-11 7-11-7-11-7z M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z',
  filter:  'M3 4h18l-7 9v6l-4 2v-8z',
  compare: 'M4 5h7v14H4zM13 5h7v14h-7z',
  bolt:    'M13 2 3 14h7l-1 8 10-12h-7z',
};

function Icon({ name, size = 16, fill, style, strokeWidth = 1.8 }) {
  const d = ICONS[name] || '';
  const solid = (name === 'star' || name === 'play');
  return (
    <svg width={size} height={size} viewBox="0 0 24 24"
      fill={solid ? (fill || 'currentColor') : 'none'}
      stroke={solid ? 'none' : 'currentColor'} strokeWidth={strokeWidth}
      strokeLinecap="round" strokeLinejoin="round" style={style}>
      <path d={d} />
    </svg>
  );
}

function Stars({ value, onSet, size = 22 }) {
  return (
    <div className="stars" onMouseLeave={() => {}}>
      {[1, 2, 3, 4, 5].map((n) => (
        <svg key={n} className={'star' + (n <= value ? ' on' : '')} viewBox="0 0 24 24" width={size} height={size}
          fill="currentColor"
          onClick={(e) => { e.stopPropagation(); onSet(n === value ? 0 : n); }}>
          <path d={ICONS.star} />
        </svg>
      ))}
    </div>
  );
}

function MiniStars({ value }) {
  if (!value) return null;
  return (
    <div className="ministars">
      {Array.from({ length: value }).map((_, i) => (
        <svg key={i} viewBox="0 0 24 24" fill="currentColor"><path d={ICONS.star} /></svg>
      ))}
    </div>
  );
}

function fmtTime(ms) {
  const d = new Date(ms);
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}
function fmtClock(secs) {
  if (secs <= 0 || !isFinite(secs)) return '—';
  const m = Math.floor(secs / 60), s = Math.floor(secs % 60);
  return m + ':' + String(s).padStart(2, '0');
}

const CLUSTER_COLOR = {
  wall:   '#c08a4e', tower: '#5b9bd5', oak: '#5bbf6a',
  harbor: '#e08a52', cafe:  '#caa15e', street: '#8893a6',
};

Object.assign(window, { Icon, Stars, MiniStars, fmtTime, fmtClock, CLUSTER_COLOR,
  useState, useEffect, useRef, useMemo, useCallback });
