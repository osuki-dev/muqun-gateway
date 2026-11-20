// WebKit exposes the initial site icon natively but may ignore later link edits.
// This observer runs only in the top frame, in a private WebKit script world.
(() => {
    function locationOfIcon() {
        const icons = document.head?.querySelectorAll('link[rel~="icon" i][href]');
        return icons?.length ? icons[icons.length - 1].href : "";
    }
    const report = data => window.webkit.messageHandlers.browserIcon.postMessage(JSON.stringify({url:location.href, ...data}));
    let lastTitle;
    function updateTitle() {
        if (document.title === lastTitle) return;
        lastTitle = document.title;
        report({title: lastTitle.slice(0, 512)});
    }
    updateTitle();
    for (const event of ["pageshow", "popstate", "hashchange"]) {
        addEventListener(event, () => { lastTitle = undefined; updateTitle(); });
    }
    addEventListener("pageshow", event => {
        if (event.persisted && locationOfIcon()) read(++revision, locationOfIcon());
    });
    let current = locationOfIcon();
    let timer;
    let revision = 0;
    function publish(serial, pixels) {
        if (serial !== revision) return;
        report({pixels});
    }
    async function read(serial, href) {
        if (!href) return publish(serial, null);
        try {
            const url = new URL(href);
            if (!(["http:", "https:"].includes(url.protocol) || /^data:image\//i.test(href))
                || url.username || url.password) return publish(serial, null);
            const image = new Image();
            image.crossOrigin = "anonymous";
            image.src = url.href;
            await image.decode();
            if (serial !== revision) return;
            const canvas = document.createElement("canvas");
            canvas.width = canvas.height = 32;
            const graphics = canvas.getContext("2d");
            const scale = 32 / Math.max(image.naturalWidth, image.naturalHeight);
            const w = image.naturalWidth * scale, h = image.naturalHeight * scale;
            graphics.drawImage(image, (32 - w) / 2, (32 - h) / 2, w, h);
            publish(serial, Array.from(graphics.getImageData(0, 0, 32, 32).data));
        } catch { publish(serial, null); }
    }
    const head = document.head;
    if (!head) return;
    new MutationObserver(() => {
        updateTitle();
        const next = locationOfIcon();
        if (next === current) return;
        current = next;
        const serial = ++revision;
        clearTimeout(timer);
        timer = setTimeout(() => read(serial, next), 75);
    }).observe(head, {subtree:true, characterData:true, childList:true, attributes:true, attributeFilter:["rel","href"]});
    addEventListener("pagehide", () => { ++revision; clearTimeout(timer); });
})();
