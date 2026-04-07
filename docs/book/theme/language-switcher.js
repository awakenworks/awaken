(function () {
    if (document.body.classList.contains("sidebar-iframe-inner")) {
        return;
    }

    const menuButtons = document.querySelector("#mdbook-menu-bar .right-buttons");
    if (!menuButtons || document.querySelector(".language-switcher")) {
        return;
    }

    const currentLang = document.documentElement.lang === "zh-CN" ? "zh-CN" : "en";
    const pathToRoot = typeof path_to_root === "string" && path_to_root.length > 0 ? path_to_root : ".";
    const bookRootUrl = new URL(pathToRoot, window.location.href);
    let bookRootPath = bookRootUrl.pathname;

    if (!bookRootPath.endsWith("/")) {
        bookRootPath += "/";
    }

    const currentPath = window.location.pathname;
    let pagePath = currentPath.startsWith(bookRootPath)
        ? currentPath.slice(bookRootPath.length)
        : "";

    if (pagePath === "index.html") {
        pagePath = "";
    }

    let alternateRootPath;
    if (currentLang === "zh-CN") {
        alternateRootPath = bookRootPath.replace(/zh-CN\/?$/, "");
    } else {
        alternateRootPath = `${bookRootPath.replace(/\/$/, "")}/zh-CN/`;
    }

    const alternateUrl = new URL(
        `${pagePath}${window.location.search}${window.location.hash}`,
        `${window.location.origin}${alternateRootPath}`
    );

    const switcher = document.createElement("nav");
    switcher.className = "language-switcher";
    switcher.setAttribute("aria-label", "Language switcher");

    const entries = [
        { code: "en", label: "EN", href: currentLang === "en" ? window.location.href : alternateUrl.href },
        { code: "zh-CN", label: "中文", href: currentLang === "zh-CN" ? window.location.href : alternateUrl.href },
    ];

    entries.forEach((entry) => {
        const link = document.createElement("a");
        link.className = "language-switcher-link";
        if (entry.code === currentLang) {
            link.classList.add("active");
            link.setAttribute("aria-current", "page");
        }
        link.href = entry.href;
        link.textContent = entry.label;
        switcher.appendChild(link);
    });

    menuButtons.prepend(switcher);
})();
