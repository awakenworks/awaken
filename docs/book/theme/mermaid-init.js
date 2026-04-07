// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

(() => {
    const darkThemes = ["ayu", "navy", "coal"];
    const lightThemes = ["light", "rust"];
    const classList = document.documentElement.classList;

    let lastThemeWasLight = true;
    for (const cssClass of classList) {
        if (darkThemes.includes(cssClass)) {
            lastThemeWasLight = false;
            break;
        }
    }

    const theme = lastThemeWasLight ? "default" : "dark";
    mermaid.initialize({ startOnLoad: true, theme });

    // Refresh the page after a theme flip so Mermaid diagrams rerender with the new palette.
    const bindReload = (themeName, shouldReload) => {
        const button =
            document.getElementById(`mdbook-theme-${themeName}`) ||
            document.getElementById(themeName);

        if (!button) {
            return;
        }

        button.addEventListener("click", () => {
            if (shouldReload) {
                window.location.reload();
            }
        });
    };

    for (const darkTheme of darkThemes) {
        bindReload(darkTheme, lastThemeWasLight);
    }

    for (const lightTheme of lightThemes) {
        bindReload(lightTheme, !lastThemeWasLight);
    }
})();
