
(function () {
  function escapeHtml(s) {
    return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }
  function fmtTime(s) {
    s = Math.max(0, Math.floor(s || 0));
    var m = Math.floor(s / 60), sec = s % 60;
    return m + ":" + (sec < 10 ? "0" : "") + sec;
  }
  function renderQueue(queue) {
    if (!queue || !queue.length) return '<p class="empty">Queue is empty.</p>';
    var shown = queue.slice(0, 8).map(function (q, i) {
      return '<li><span class="rank">' + (i + 1) + '</span>' +
             '<span class="who">' + escapeHtml(q.title || "Unknown title") + '</span>' +
             '<span class="pts">' + escapeHtml(q.requester_name || "") + '</span></li>';
    }).join("");
    var more = queue.length > 8 ? '<p class="help">+' + (queue.length - 8) + ' more</p>' : "";
    return '<ol class="leaderboard">' + shown + '</ol>' + more;
  }

  var lastPayload = null, lastAt = 0;

  function paint(data, elapsed) {
    var body = document.getElementById("np-body");
    if (body) {
      if (!data.playing) {
        body.innerHTML = '<p class="empty">Nothing playing right now.</p>' + renderQueue(data.queue);
      } else {
        var dur = data.duration_seconds || 0;
        var pct = dur > 0 ? Math.min(100, (elapsed / dur) * 100) : 0;
        body.innerHTML =
          '<div class="np-track">' +
            '<div class="np-title">' + escapeHtml(data.title) + '</div>' +
            '<div class="np-meta">requested by ' + escapeHtml(data.requester_name) + '</div>' +
            '<div class="np-bar"><div class="np-fill" style="width:' + pct + '%"></div></div>' +
            '<div class="np-time"><span>' + fmtTime(elapsed) + '</span><span>' + fmtTime(dur) + '</span></div>' +
          '</div>' +
          '<h3>Up next (' + (data.queue_size || 0) + ')</h3>' + renderQueue(data.queue);
      }
    }

    var chipState = document.getElementById("chip-state");
    if (chipState && data.state) {
      chipState.textContent = data.state;
      chipState.className = "chip state-" + data.state;
    }
    var chipQueue = document.getElementById("chip-queue");
    if (chipQueue) chipQueue.textContent = (data.queue_size || 0) + " queued";
    var chipNp = document.getElementById("chip-np");
    if (chipNp) {
      if (data.playing) {
        chipNp.style.display = "";
        chipNp.title = data.title || "";
        chipNp.textContent = "▶ " + (data.title || "");
      } else {
        chipNp.style.display = "none";
      }
    }
  }

  function onPayload(data) {
    lastPayload = data;
    lastAt = performance.now();
    paint(data, data.playing ? (data.elapsed_seconds || 0) : 0);
  }

  // Ticks between server pushes so the progress bar and elapsed time move
  // smoothly instead of only jumping once a second when the overlay's own
  // push happens to land. Drift-corrected against the wall clock each
  // tick rather than just incrementing a counter, so a delayed tick (a
  // slow tab, a backgrounded browser) catches back up instead of running
  // permanently behind.
  setInterval(function () {
    if (lastPayload && lastPayload.playing) {
      var drift = (performance.now() - lastAt) / 1000;
      paint(lastPayload, (lastPayload.elapsed_seconds || 0) + drift);
    }
  }, 250);

  var ws = null;
  function connectWs() {
    var proto = location.protocol === "https:" ? "wss:" : "ws:";
    try {
      ws = new WebSocket(proto + "//" + location.host + "/ws/nowplaying");
    } catch (e) {
      return;
    }
    ws.onmessage = function (ev) {
      try { onPayload(JSON.parse(ev.data)); } catch (e) {}
    };
    ws.onclose = function () { setTimeout(connectWs, 2000); };
    ws.onerror = function () { try { ws.close(); } catch (e) {} };
  }

  // Fallback for a proxy/browser that blocks websockets outright — polls
  // only while the socket isn't actually open, so this never fights the
  // websocket for which value wins once it connects.
  function pollFallback() {
    if (ws && ws.readyState === WebSocket.OPEN) return;
    fetch("/nowplaying.json").then(function (r) { return r.json(); }).then(onPayload).catch(function () {});
  }

  connectWs();
  pollFallback();
  setInterval(pollFallback, 3000);

  // Local uptime ticker — the "up Xh Ym" chip only needs to look alive,
  // not be pushed from the server every second for that.
  var uptimeEl = document.getElementById("chip-uptime");
  if (uptimeEl) {
    var base = parseInt(uptimeEl.dataset.uptimeBase || "0", 10);
    var start = performance.now();
    setInterval(function () {
      var total = base + Math.floor((performance.now() - start) / 1000);
      var h = Math.floor(total / 3600), m = Math.floor((total % 3600) / 60);
      uptimeEl.textContent = h ? ("up " + h + "h " + m + "m") : ("up " + m + "m");
    }, 1000);
  }
})();
