const start = window.__SA_START__ ?? {};
document.getElementById("reason").textContent = start.reason ?? "没有拿到原因。";
document.getElementById("data-dir").textContent = start.dataDir ?? "";
