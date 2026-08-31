window.addEventListener("message", (event) => {
  if (event.data?.buzz !== "port" || event.ports.length !== 1) return;
  const port = event.ports[0];
  port.start();
  document.getElementById("status").textContent = "Buzz bridge connected";
});
parent.postMessage({ buzz: "ready" }, "*");
