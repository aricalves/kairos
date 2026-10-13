// The script of the rendered docs sites (KAIROS-T-0404). It draws each
// mermaid fence (<pre class="kairos-mermaid"> with the escaped source) with
// the vendored Mermaid, loaded only when a page has one, with
// securityLevel "strict", in the light or dark theme. The same code as the
// loader of the app (kairos-web/index.html); a page of a site has no
// inline script.
(function () {
  var SRC = "/pages/_/mermaid.min.js";
  var loading = null;
  var count = 0;
  function load() {
    if (!loading) {
      loading = new Promise(function (resolve, reject) {
        var s = document.createElement("script");
        s.src = SRC;
        s.onload = function () { resolve(window.mermaid); };
        s.onerror = function () { loading = null; reject(new Error("Kairos could not load " + SRC + ".")); };
        document.head.appendChild(s);
      });
    }
    return loading;
  }
  function dark() {
    return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
  }
  function fail(el, error) {
    el.classList.add("kairos-mermaid--error");
    var note = document.createElement("div");
    note.className = "kairos-mermaid__error";
    note.textContent = "The diagram has an error: " + ((error && error.message) || String(error));
    el.parentNode && el.parentNode.insertBefore(note, el);
  }
  function draw() {
    var nodes = Array.prototype.slice.call(document.querySelectorAll("pre.kairos-mermaid"));
    if (!nodes.length) return;
    load().then(function (mermaid) {
      mermaid.initialize({ startOnLoad: false, securityLevel: "strict", theme: dark() ? "dark" : "default" });
      nodes.forEach(function (el) {
        var id = "kairos-mermaid-" + (++count);
        mermaid.render(id, el.textContent).then(function (result) {
          var div = document.createElement("div");
          div.className = "kairos-mermaid-diagram";
          div.innerHTML = result.svg;
          el.replaceWith(div);
        }, function (error) {
          var stray = document.getElementById("d" + id);
          if (stray) stray.remove();
          fail(el, error);
        });
      });
    }, function (error) {
      nodes.forEach(function (el) { fail(el, error); });
    });
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", draw);
  } else {
    draw();
  }
})();
