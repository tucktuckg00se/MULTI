// MULTI viewer: plays /hls/<name>/master.m3u8 with a caption language picker.
(function () {
  'use strict';
  const name = decodeURIComponent(location.pathname.split('/').filter(Boolean).pop() || '');
  const src = '/hls/' + encodeURIComponent(name) + '/master.m3u8';
  const video = document.getElementById('v');
  const pick = document.getElementById('lang');
  const state = document.getElementById('state');
  document.title = 'MULTI live: ' + name;
  const say = (t) => { state.textContent = t; };

  function fill(tracks, current) {
    pick.length = 1;
    tracks.forEach((t, i) => {
      const o = document.createElement('option');
      o.value = String(i);
      o.textContent = t.name || t.lang || ('Track ' + (i + 1));
      pick.appendChild(o);
    });
    pick.value = String(current);
  }

  if (window.Hls && Hls.isSupported()) {
    let hls = null;
    let picked = false;
    const start = () => {
      if (hls) hls.destroy();
      // Embedded 608/708 would show up as extra tracks; WebVTT carries every language.
      hls = new Hls({ enableCEA708Captions: false, liveSyncDurationCount: 3, subtitleDisplay: true });
      hls.on(Hls.Events.MANIFEST_PARSED, () => { say('Live'); video.play().catch(() => {}); });
      hls.on(Hls.Events.SUBTITLE_TRACKS_UPDATED, () => {
        // ?lang=xx picks a language once; otherwise the playlist's default (the source).
        setTimeout(() => {
          const tracks = hls.subtitleTracks;
          const want = new URLSearchParams(location.search).get('lang');
          const i = want && !picked ? tracks.findIndex((t) => t.lang === want) : -1;
          const def = tracks.findIndex((t) => t.default);
          if (i >= 0) { hls.subtitleTrack = i; picked = true; } else if (hls.subtitleTrack < 0 && def >= 0) hls.subtitleTrack = def;
          fill(tracks, hls.subtitleTrack);
        }, 0);
      });
      hls.on(Hls.Events.SUBTITLE_TRACK_SWITCH, (_, d) => { pick.value = String(d.id); });
      hls.on(Hls.Events.ERROR, (_, d) => {
        if (!d.fatal) return;
        say('Waiting for the stream…');
        setTimeout(start, 3000);
      });
      hls.loadSource(src);
      hls.attachMedia(video);
    };
    pick.onchange = () => { if (hls) hls.subtitleTrack = Number(pick.value); };
    setInterval(() => {
      if (hls && hls.latency > 0) say('Live · ' + hls.latency.toFixed(1) + ' s behind');
    }, 1000);
    start();
  } else if (video.canPlayType('application/vnd.apple.mpegurl')) {
    // Safari: native HLS; its text tracks are the WebVTT renditions.
    video.src = src;
    const sync = () => {
      const tracks = Array.from(video.textTracks).filter((t) => t.kind === 'subtitles' || t.kind === 'captions');
      fill(tracks.map((t) => ({ name: t.label, lang: t.language })), tracks.findIndex((t) => t.mode === 'showing'));
      pick.onchange = () => tracks.forEach((t, i) => { t.mode = i === Number(pick.value) ? 'showing' : 'disabled'; });
    };
    video.textTracks.addEventListener('addtrack', sync);
    video.addEventListener('playing', () => say('Live'));
  } else {
    say('This browser cannot play HLS.');
  }
})();
