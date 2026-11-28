browser.runtime.onMessage.addListener((message) => {
    if (message?.type !== "watch-history-request") return;
    return waitForStreamTube();
});

function waitForStreamTube() {
    return new Promise((resolve) => {
        const port = browser.runtime.connectNative("io.github.avaneesh-inamdar.StreamTube.SafariPrototype");
        let finished = false;
        const timeout = setTimeout(() => finish(null), 6000);

        function finish(request) {
            if (finished) return;
            finished = true;
            clearTimeout(timeout);
            resolve(request);
            port.disconnect();
        }

        port.onMessage.addListener((message) => {
            const payload = message?.userInfo ?? message;
            if (/^[1-9][0-9]{0,4}$/.test(payload?.port) && Number(payload.port) <= 65535
                    && /^[A-Za-z0-9_-]{43}$/.test(payload?.token)
                    && payload.purpose === "history-and-reactions") {
                finish({ port: payload.port, token: payload.token, purpose: payload.purpose });
            }
        });
        port.onDisconnect.addListener(() => finish(null));
        port.postMessage({ type: "ready-for-watch-history" });
    });
}
