// Suite Hub client. Renders the roster into the rail and swaps the stage
// between a tool's own UI (iframe) and a status card. No framework, no deps.
(function () {
  "use strict";

  var toolsEl = document.getElementById("tools");
  var stageEl = document.getElementById("stage");
  var scannedEl = document.getElementById("scanned");
  var selected = null; // tool id
  var lastRoster = [];

  function el(tag, attrs, kids) {
    var n = document.createElement(tag);
    Object.keys(attrs || {}).forEach(function (k) {
      if (k === "class") n.className = attrs[k];
      else if (k === "text") n.textContent = attrs[k];
      else n.setAttribute(k, attrs[k]);
    });
    (kids || []).forEach(function (c) { n.appendChild(c); });
    return n;
  }

  function find(id) {
    for (var i = 0; i < lastRoster.length; i++) {
      if (lastRoster[i].id === id) return lastRoster[i];
    }
    return null;
  }

  /** The one tool we should show on load: first healthy one with a framable UI. */
  function firstShowable(tools) {
    for (var i = 0; i < tools.length; i++) {
      if (tools[i].presence !== "absent" && tools[i].ui) return tools[i].id;
    }
    return tools.length ? tools[0].id : null;
  }

  function renderRail(tools) {
    toolsEl.textContent = "";
    tools.forEach(function (t) {
      var sub =
        t.presence === "absent"
          ? t.reason || "absent"
          : t.ui
            ? t.uiFrameable === false
              ? t.uiReason || "opens in a new tab"
              : t.uiReachable === false
                ? "UI not running"
                : t.blurb
            : "no web UI";

      var name = el("span", { class: "name" }, [
        el("b", { text: t.label }),
        el("span", { class: "ver", text: t.version ? "v" + t.version : "" }),
      ]);

      var btn = el("button", { class: "tool", type: "button" }, [
        el("span", { class: "dot " + t.presence, "aria-hidden": "true" }),
        el("span", {}, [name, el("span", { class: "sub", text: sub })]),
      ]);
      btn.setAttribute("aria-current", String(t.id === selected));
      btn.addEventListener("click", function () { select(t.id); });

      toolsEl.appendChild(el("li", {}, [btn]));
    });
  }

  function card(kids) {
    var c = el("div", { class: "card" }, kids);
    stageEl.textContent = "";
    stageEl.appendChild(c);
  }

  function checksList(checks) {
    var ul = el("ul", { class: "checks" });
    checks.forEach(function (c) {
      ul.appendChild(
        el("li", {}, [
          el("span", { class: "k " + (c.ok ? "good" : "bad"), text: (c.ok ? "OK   " : "FAIL ") + c.name }),
          el("span", { class: "d", text: c.detail }),
        ]),
      );
    });
    return ul;
  }

  function renderStage(t) {
    if (!t) {
      card([
        el("p", { class: "eyebrow", text: "Suite Hub" }),
        el("h1", { text: "No suite tools found" }),
        el("p", { text: "Install a tool and it will appear here on the next scan." }),
      ]);
      return;
    }

    if (t.presence === "absent") {
      card([
        el("p", { class: "eyebrow", text: t.id + " · absent" }),
        el("h1", { text: t.label + " isn't answering" }),
        el("p", { text: t.reason || "Not installed." }),
        el("p", { text: t.blurb + "." }),
        el("a", { class: "btn", href: t.install, target: "_blank", rel: "noreferrer", text: "Install " + t.label }),
      ]);
      return;
    }

    // Present, but nothing to frame: show its health instead.
    if (!t.ui || t.uiReachable === false || t.uiFrameable === false) {
      var kids = [
        el("p", { class: "eyebrow", text: t.id + " · " + t.presence + (t.version ? " · v" + t.version : "") }),
        el("h1", { text: t.label }),
      ];
      if (!t.ui) {
        kids.push(el("p", { text: t.blurb + ". This tool serves no web UI." }));
      } else if (t.uiReachable === false) {
        kids.push(el("p", { text: "Installed and healthy, but nothing is serving " + t.ui + " right now." }));
      } else {
        kids.push(el("p", { text: t.uiReason || "This UI refuses to be framed." }));
        kids.push(el("a", { class: "btn", href: t.ui, target: "_blank", rel: "noreferrer", text: "Open in a new tab" }));
      }
      if (t.presence === "unhealthy") {
        kids.push(el("p", { text: "Its own checks are failing:" }));
      }
      if (t.checks && t.checks.length) kids.push(checksList(t.checks));
      card(kids);
      return;
    }

    // Frame the tool's own UI, at the tool's own port. No proxying.
    stageEl.textContent = "";
    stageEl.appendChild(
      el("iframe", { src: t.ui, title: t.label, sandbox: "allow-scripts allow-forms allow-same-origin allow-popups" }),
    );
  }

  function select(id) {
    selected = id;
    renderRail(lastRoster);
    renderStage(find(id));
  }

  function apply(roster) {
    lastRoster = roster.tools;
    scannedEl.textContent =
      roster.tools.filter(function (t) { return t.presence !== "absent"; }).length +
      " of " + roster.tools.length + " present";
    if (!selected || !find(selected)) selected = firstShowable(lastRoster);
    renderRail(lastRoster);
    renderStage(find(selected));
  }

  function refresh() {
    fetch("/api/roster", { headers: { accept: "application/json" } })
      .then(function (r) { return r.json(); })
      .then(function (roster) {
        // Re-render the rail always; only rebuild the stage if the selection
        // changed state, so we don't reload a healthy iframe every 10s.
        var before = find(selected);
        lastRoster = roster.tools;
        scannedEl.textContent =
          roster.tools.filter(function (t) { return t.presence !== "absent"; }).length +
          " of " + roster.tools.length + " present";
        if (!selected || !find(selected)) { apply(roster); return; }
        var after = find(selected);
        renderRail(lastRoster);
        if (!before || before.presence !== after.presence || before.ui !== after.ui ||
            before.uiReachable !== after.uiReachable || before.uiFrameable !== after.uiFrameable) {
          renderStage(after);
        }
      })
      .catch(function () { /* hub restarting; next tick will pick it up */ });
  }

  fetch("/api/roster")
    .then(function (r) { return r.json(); })
    .then(apply)
    .catch(function () {
      card([el("h1", { text: "Hub is not responding" })]);
    });

  setInterval(refresh, 10000);
})();
