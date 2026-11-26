// Reports CSS cursor changes from the top frame to the macOS helper.
(() => {
    const post = name => {
        try {
            window.webkit.messageHandlers.muqunCursor.postMessage(name);
        } catch {}
    };
    let last = "default";
    let queued = false;
    let lastX = -1;
    let lastY = -1;
    function inspect() {
        queued = false;
        const el = document.elementFromPoint(lastX, lastY);
        let name = "default";
        if (el) {
            const raw = (getComputedStyle(el).cursor || "").split(",")[0].trim();
            if (raw && raw !== "auto") {
                name = raw;
            } else if (
                el.isContentEditable ||
                el.tagName === "INPUT" ||
                el.tagName === "TEXTAREA"
            ) {
                name = "text";
            } else if (el.closest("a[href], button, [role='button'], summary, label")) {
                name = "pointer";
            }
        }
        if (name !== last) {
            last = name;
            post(name);
        }
    }
    addEventListener(
        "mousemove",
        e => {
            lastX = e.clientX;
            lastY = e.clientY;
            if (!queued) {
                queued = true;
                requestAnimationFrame(inspect);
            }
        },
        { passive: true, capture: true },
    );
})();
