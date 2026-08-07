// Medium-fixture script: small but real DOM work so the load exercises JS
// fetch + parse + execute. Marks the page so the DOM dump proves JS ran.
(function () {
  function ran() {
    var status = document.getElementById("js-status");
    status.textContent = "js-ran";
    status.classList.add("ran");

    // Build a small list dynamically (DOM mutation after parse).
    var list = document.createElement("ul");
    for (var i = 0; i < 20; i++) {
      var li = document.createElement("li");
      li.textContent = "generated item " + i;
      list.appendChild(li);
    }
    document.getElementById("generated").appendChild(list);
    window.__benchJsRan = true;
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", ran);
  } else {
    ran();
  }
})();
