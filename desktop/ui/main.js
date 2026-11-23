const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);
const $ = (id) => document.getElementById(id);

// 由桌面外壳调：换成 state.view 说的那一屏。
window.saShow = (state) => {
  for (const view of ["choose", "connecting", "failed"]) {
    $(view).hidden = view !== state.view;
  }
  for (const el of document.querySelectorAll(".server")) {
    el.textContent = state.server ?? "";
  }
  const remote = Boolean(state.server);
  $("failed-title").textContent = remote ? "没能连上服务器" : "苦乐没能启动";
  $("reason").textContent = state.reason ?? "没有拿到原因。";
  $("local-hints").hidden = remote;
  $("server-hints").hidden = !remote;
  // 用 --workdir、--server 启动的换不了。
  $("switch").hidden = Boolean(state.pinned);
  $("data-dir").textContent = state.dataDir ?? "";
  $("choose-error").hidden = true;
};

const showError = (error) => {
  $("choose-error").textContent = String(error);
  $("choose-error").hidden = false;
};

$("local").addEventListener("click", () => invoke("choose_local").catch(showError));
$("server-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const port = $("port").value.trim();
  invoke("choose_server", {
    host: $("host").value,
    port: port === "" ? null : Number(port),
  }).catch(showError);
});
$("retry").addEventListener("click", () => invoke("retry").catch(showError));
$("switch").addEventListener("click", () => window.saShow({ view: "choose" }));

window.saShow(window.__SA_START__ ?? { view: "failed" });
