// 启动页：只在本机服务起不来时出现，说明原因。连服务器在主界面的「连接」面板里。
const invoke = (command) => window.__TAURI__.core.invoke(command);
const $ = (id) => document.getElementById(id);

const state = window.__SA_START__ ?? {};
$("reason").textContent = state.reason ?? "没有拿到原因。";
$("data-dir").textContent = state.dataDir ?? "";
// 用 --workdir 启动的换不了。
$("pick").hidden = Boolean(state.pinned);

const run = (command) =>
  invoke(command).catch((error) => {
    $("error").textContent = String(error);
    $("error").hidden = false;
  });
$("retry").addEventListener("click", () => run("retry"));
$("pick").addEventListener("click", () => run("pick_workdir"));
