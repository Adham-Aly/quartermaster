// The plan page's script. `buildReply` is pure and global so a test can run it without a page.

function quoteLines(text) {
  return String(text).replace(/\r\n?/g, "\n").replace(/\s+$/, "").split("\n").map(function (line) {
    return line === "" ? ">" : "> " + line;
  });
}

function questionAnswered(q, s) {
  if (!s) return false;
  if (q.open) return !!(s.answer && s.answer.trim());
  return (s.choices || []).length > 0 || !!(s.other && s.other.trim());
}

// `state` is { questions: { id: { choices: [ids], other, answer, note } },
// sections: { id: { change, ask } } }; `meta` is the page's JSON block.
function buildReply(state, meta) {
  var qs = (state && state.questions) || {};
  var ss = (state && state.sections) || {};
  var answered = 0;
  meta.questions.forEach(function (q) { if (questionAnswered(q, qs[q.id])) answered += 1; });
  var out = ['<plan-reply task="' + meta.task + '" plan="' + meta.plan + '" round="' + meta.round + '">'];
  var total = meta.questions.length;
  out.push("Reply to round " + meta.round + " of the plan for " + meta.task +
    (total ? " (" + answered + " of " + total + " question" + (total === 1 ? "" : "s") + " answered)." : "."));
  meta.questions.forEach(function (q) {
    var s = qs[q.id] || {};
    out.push("");
    out.push("## Q" + q.n + " " + q.id);
    var chosen = q.choices.filter(function (c) { return (s.choices || []).indexOf(c.id) >= 0; });
    if (chosen.length) {
      out.push("Choice: " + chosen.map(function (c) { return c.id; }).join(", ") +
        " (" + chosen.map(function (c) { return c.label.replace(/[\r\n]+/g, " "); }).join(", ") + ")");
    }
    if (q.other && s.other && s.other.trim()) { out.push("Other:"); out.push.apply(out, quoteLines(s.other)); }
    if (q.open && s.answer && s.answer.trim()) { out.push("Answer:"); out.push.apply(out, quoteLines(s.answer)); }
    if (!questionAnswered(q, s)) out.push("Left open.");
    if (s.note && s.note.trim()) { out.push("Note:"); out.push.apply(out, quoteLines(s.note)); }
  });
  meta.sections.forEach(function (sec) {
    var s = ss[sec.id];
    if (!s || !((s.change && s.change.trim()) || (s.ask && s.ask.trim()))) return;
    out.push("");
    out.push("## Section " + sec.id + " (" + sec.title.replace(/[\r\n]+/g, " ") + ")");
    if (s.change && s.change.trim()) { out.push("Change:"); out.push.apply(out, quoteLines(s.change)); }
    if (s.ask && s.ask.trim()) { out.push("Ask:"); out.push.apply(out, quoteLines(s.ask)); }
  });
  out.push("</plan-reply>");
  return out.join("\n") + "\n";
}

if (typeof document !== "undefined") (function () {
  var meta = JSON.parse(document.getElementById("plan-meta").textContent);
  var closed = meta.state === "approved" || meta.state === "locked";
  var key = "quartermaster.plan." + meta.plan + ".r" + meta.round;
  var sentKey = key + ".sent";
  var state = { questions: {}, sections: {} };
  function store() { try { return window.localStorage; } catch (e) { return null; } }

  // Theme.
  document.getElementById("theme-toggle").addEventListener("click", function () {
    var root = document.documentElement;
    var next = root.getAttribute("data-theme") === "dark" ? "light" : "dark";
    root.setAttribute("data-theme", next);
    var s = store(); if (s) try { s.setItem("quartermaster.plan.theme", next); } catch (e) {}
  });
  document.getElementById("print").addEventListener("click", function () { window.print(); });

  // Maths.
  if (window.katex) {
    document.querySelectorAll(".math").forEach(function (el) {
      try {
        window.katex.render(el.textContent, el, { displayMode: el.getAttribute("data-display") === "true", throwOnError: false });
      } catch (e) {}
    });
  }

  if (closed) {
    document.body.classList.add("closed");
    document.querySelectorAll(".plan button, .plan textarea").forEach(function (el) { el.disabled = true; });
    return;
  }

  var send = meta.submit ? document.getElementById("send") : null;
  var sendStatus = document.getElementById("send-status");
  var sending = false, sendShut = false;

  // Saved work and the sent mark for this round only; older rounds' are dropped.
  var s = store();
  if (s) {
    try {
      var prefix = "quartermaster.plan." + meta.plan + ".";
      for (var i = s.length - 1; i >= 0; i -= 1) {
        var k = s.key(i);
        if (k && k.indexOf(prefix) === 0 && k !== key && k !== sentKey) s.removeItem(k);
      }
      var saved = JSON.parse(s.getItem(key) || "null");
      if (saved && saved.questions && saved.sections) state = saved;
    } catch (e) {}
  }
  function save() { if (s) try { s.setItem(key, JSON.stringify(state)); } catch (e) {} refresh(); }
  function qState(id) { return state.questions[id] || (state.questions[id] = { choices: [], other: "", answer: "", note: "" }); }
  function secState(id) { return state.sections[id] || (state.sections[id] = { change: "", ask: "" }); }
  var byId = {};
  meta.questions.forEach(function (q) { byId[q.id] = q; });

  function paint() {
    document.querySelectorAll(".q").forEach(function (box) {
      var q = byId[box.getAttribute("data-q")]; if (!q) return;
      var st = qState(q.id);
      box.querySelectorAll(".choice").forEach(function (b) {
        var c = b.getAttribute("data-choice");
        var on = c === "" ? !!st.otherOn : st.choices.indexOf(c) >= 0;
        b.setAttribute("aria-pressed", on ? "true" : "false");
      });
      var other = box.querySelector(".q-other");
      if (other) { other.hidden = !st.otherOn; other.value = st.other || ""; }
      var answer = box.querySelector(".q-answer"); if (answer) answer.value = st.answer || "";
      var note = box.querySelector(".q-note");
      if (note) { note.value = st.note || ""; if (st.note) note.hidden = false; }
    });
    document.querySelectorAll(".sec-tools").forEach(function (tools) {
      var st = state.sections[tools.getAttribute("data-section")];
      if (!st) return;
      if (st.change) box(tools, "change").value = st.change;
      if (st.ask) box(tools, "ask").value = st.ask;
    });
  }

  function refresh() {
    var answered = 0;
    meta.questions.forEach(function (q) {
      var done = questionAnswered(q, state.questions[q.id]);
      if (done) answered += 1;
      var el = document.getElementById("q-" + q.id); if (el) el.classList.toggle("answered", done);
    });
    document.querySelectorAll(".outline li").forEach(function (li) {
      var a = li.querySelector("a"); if (!a) return;
      var sec = a.getAttribute("href").slice(1);
      var mine = meta.questions.filter(function (q) { return q.section === sec; });
      li.classList.toggle("done", mine.length > 0 && mine.every(function (q) { return questionAnswered(q, state.questions[q.id]); }));
    });
    var notes = meta.sections.filter(function (sec) {
      var st = state.sections[sec.id]; return st && ((st.change || "").trim() || (st.ask || "").trim());
    }).length;
    var parts = [];
    if (meta.questions.length) parts.push(answered + " of " + meta.questions.length + " answered");
    if (notes) parts.push(notes + " section note" + (notes === 1 ? "" : "s"));
    document.getElementById("reply-count").textContent = parts.join(" · ");
    var rec = document.getElementById("take-recommended");
    rec.hidden = !meta.questions.some(function (q) { return q.choices.some(function (c) { return c.recommended; }); });
    document.getElementById("copy").disabled = !(answered || notes);
    if (send) send.disabled = sending || sendShut || !(answered || notes);
  }

  function box(tools, kind) {
    var existing = tools.parentNode.querySelector('.sec-box[data-for="' + tools.getAttribute("data-section") + '"][data-kind="' + kind + '"]');
    if (existing) return existing;
    var area = document.createElement("textarea");
    area.className = "sec-box";
    area.rows = 3;
    area.setAttribute("data-for", tools.getAttribute("data-section"));
    area.setAttribute("data-kind", kind);
    area.placeholder = kind === "change" ? "What should change in “" + tools.getAttribute("data-title") + "”?" : "Your question about “" + tools.getAttribute("data-title") + "”";
    area.addEventListener("input", function () { secState(area.getAttribute("data-for"))[kind] = area.value; save(); });
    tools.parentNode.insertBefore(area, tools.nextSibling);
    return area;
  }

  document.addEventListener("click", function (ev) {
    var t = ev.target.closest ? ev.target.closest("button") : null; if (!t) return;
    var qbox = t.closest(".q");
    if (t.classList.contains("choice") && qbox) {
      var q = byId[qbox.getAttribute("data-q")]; var st = qState(q.id);
      var c = t.getAttribute("data-choice");
      if (c === "") {
        st.otherOn = !st.otherOn;
        if (!q.multiple && st.otherOn) st.choices = [];
        if (!st.otherOn) st.other = "";
      } else if (q.multiple) {
        var at = st.choices.indexOf(c);
        if (at >= 0) st.choices.splice(at, 1); else st.choices.push(c);
      } else {
        st.choices = st.choices[0] === c ? [] : [c];
        st.otherOn = false; st.other = "";
      }
      paint(); save();
      if (c === "" && st.otherOn) qbox.querySelector(".q-other").focus();
    } else if (t.classList.contains("q-add-note") && qbox) {
      var note = qbox.querySelector(".q-note"); note.hidden = false; note.focus();
    } else if (t.classList.contains("sec-change") || t.classList.contains("sec-ask")) {
      box(t.closest(".sec-tools"), t.classList.contains("sec-change") ? "change" : "ask").focus();
    }
  });
  document.addEventListener("input", function (ev) {
    var t = ev.target, qbox = t.closest ? t.closest(".q") : null; if (!qbox) return;
    var st = qState(qbox.getAttribute("data-q"));
    if (t.classList.contains("q-other")) st.other = t.value;
    else if (t.classList.contains("q-answer")) st.answer = t.value;
    else if (t.classList.contains("q-note")) st.note = t.value;
    else return;
    save();
  });

  document.getElementById("take-recommended").addEventListener("click", function () {
    meta.questions.forEach(function (q) {
      if (questionAnswered(q, state.questions[q.id])) return;
      var rec = q.choices.filter(function (c) { return c.recommended; }).map(function (c) { return c.id; });
      if (rec.length) { var st = qState(q.id); st.choices = rec; st.otherOn = false; st.other = ""; }
    });
    paint(); save();
  });
  document.getElementById("clear").addEventListener("click", function () {
    if (!window.confirm("Clear every answer and note on this page?")) return;
    state = { questions: {}, sections: {} };
    document.querySelectorAll(".sec-box").forEach(function (el) { el.parentNode.removeChild(el); });
    document.querySelectorAll(".q-note").forEach(function (el) { el.hidden = true; });
    paint(); save();
  });

  var status = document.getElementById("copy-status");
  function copied(text) { status.textContent = text; setTimeout(function () { status.textContent = ""; }, 2500); }
  function fallback(text) {
    var dlg = document.getElementById("copy-fallback"), area = document.getElementById("copy-text");
    area.value = text; dlg.showModal(); area.focus(); area.select();
  }
  document.getElementById("copy-close").addEventListener("click", function () { document.getElementById("copy-fallback").close(); });
  document.getElementById("copy").addEventListener("click", function () {
    var text = buildReply(state, meta);
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(function () { copied("Copied"); }, function () { fallback(text); });
    } else {
      fallback(text);
    }
  });

  // Send to Quartermaster: only when the app gave this page its endpoint and the round's key.
  // The app answers every request with { status, message }; only "sent" means the planner has
  // the reply.
  function sendSays(text, tone) { sendStatus.textContent = text; sendStatus.className = "send-status " + tone; }
  function shutSend(text, tone) { sendShut = true; sendSays(text, tone); refresh(); }
  function lockForm() {
    document.body.classList.add("sent");
    document.querySelectorAll(".plan button, .plan textarea, #take-recommended, #clear").forEach(function (el) { el.disabled = true; });
  }
  function clock() {
    var d = new Date();
    return ("0" + d.getHours()).slice(-2) + ":" + ("0" + d.getMinutes()).slice(-2);
  }
  var unreachable = "Couldn't reach Quartermaster. Reload this page (\u2318R); your answers are saved. Or use Copy reply.";
  if (send) {
    var sentAt = null;
    if (s) try { sentAt = s.getItem(sentKey); } catch (e) {}
    if (sentAt) { lockForm(); shutSend("Sent to Quartermaster at " + sentAt + ".", "ok"); }
    send.addEventListener("click", function () {
      if (sending || sendShut) return;
      if (!window.confirm("Send your reply to round " + meta.round + "? You can't change it after sending; the planner answers with round " + (meta.round + 1) + ".")) return;
      var body = JSON.stringify({ v: 1, task: meta.task, plan: meta.plan, round: meta.round, token: meta.submit.token, reply: buildReply(state, meta) });
      sending = true; sendSays("Sending\u2026", "busy"); refresh();
      fetch(meta.submit.url, {
        method: "POST", mode: "cors", credentials: "omit", cache: "no-store", redirect: "error",
        referrerPolicy: "no-referrer", headers: { "Content-Type": "text/plain;charset=UTF-8" }, body: body
      }).then(function (r) {
        return r.json().then(function (b) { return b || {}; }, function () { return {}; }).then(function (b) {
          return { ok: r.status === 200, status: String(b.status || ""), message: String(b.message || "") };
        });
      }).then(function (res) {
        sending = false;
        if (res.ok && res.status === "sent") {
          var at = clock();
          if (s) try { s.setItem(sentKey, at); } catch (e) {}
          lockForm(); shutSend("Sent to Quartermaster at " + at + ".", "ok");
        } else if (res.status === "duplicate") {
          lockForm(); shutSend(res.message || "This round already has your reply.", "bad");
        } else if (res.status === "stale" || res.status === "closed") {
          shutSend(res.message || "This page is out of date; reopen the plan.", "bad");
        } else {
          sendSays(res.message || "Quartermaster didn't take the reply. Try again, or use Copy reply.", "bad"); refresh();
        }
      }, function () {
        sending = false; sendSays(unreachable, "bad"); refresh();
      });
    });
  }

  paint(); refresh();
})();
