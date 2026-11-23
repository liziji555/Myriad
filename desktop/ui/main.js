const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);
const $ = (id) => document.getElementById(id);

let shown = {};

// 由桌面外壳调：换成 state.view 说的那一屏。
window.saShow = (state) => {
  shown = state;
  for (const view of ["connect", "connecting", "failed"]) {
    $(view).hidden = view !== state.view;
  }
  for (const el of document.querySelectorAll(".server")) {
    el.textContent = state.server ?? "";
  }
  const mode = state.current?.mode;
  $("local-badge").hidden = mode !== "local";
  $("server-badge").hidden = mode !== "server";
  $("workdir").textContent = state.workdir ?? "第一次用时会在「文稿」下建一个「Glimmer」文件夹";
  if (state.view === "connect") {
    $("host").value = state.host ?? "";
    $("port").value = state.sshPort ?? "";
  }
  const remote = Boolean(state.server);
  $("failed-title").textContent = remote ? "没能连上服务器" : "Glimmer 没能启动";
  $("reason").textContent = state.reason ?? "没有拿到原因。";
  $("local-hints").hidden = remote;
  $("server-hints").hidden = !remote;
  // 用 --workdir、--server 启动的换不了。
  for (const id of ["switch", "failed-local", "connecting-local"]) {
    $(id).hidden = Boolean(state.pinned);
  }
  $("failed-local").hidden ||= !remote;
  $("data-dir").textContent = state.dataDir ?? "";
  for (const id of ["connect-error", "failed-error"]) $(id).hidden = true;
};

const showError = (error) => {
  const id = shown.view === "failed" ? "failed-error" : "connect-error";
  $(id).textContent = String(error);
  $(id).hidden = false;
};

const useLocal = (pick) => invoke("choose_local", { pick }).catch(showError);
$("local").addEventListener("click", () => useLocal(false));
$("local-pick").addEventListener("click", () => useLocal(true));
$("failed-local").addEventListener("click", () => useLocal(false));
$("connecting-local").addEventListener("click", () => useLocal(false));
$("server-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const port = String($("port").value).trim();
  invoke("choose_server", {
    host: $("host").value,
    port: port === "" ? null : Number(port),
  }).catch(showError);
});
$("retry").addEventListener("click", () => invoke("retry").catch(showError));
$("switch").addEventListener("click", () => window.saShow({ ...shown, view: "connect" }));

window.saShow(window.__SA_START__ ?? { view: "failed" });
