const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const popup = fs.readFileSync(path.join(__dirname, "web/connect.js"), "utf8");
const background = fs.readFileSync(path.join(__dirname, "web/background.js"), "utf8");
const token = "a".repeat(43);

async function run({
    granted = true, url = "https://www.youtube.com/", cookies = [], challenge = true,
    purpose = "history-and-reactions",
    stores = [{ id: "another-profile", tabIds: [3] }, { id: "test-profile", tabIds: [5] }],
} = {}) {
    let queried;
    let message;
    let nativeConnections = 0;
    let receiveRequest;
    let connect;
    const button = { addEventListener: (_, listener) => { connect = listener; } };
    const status = { textContent: "" };
    const context = {
        document: { getElementById: (id) => id === "connect" ? button : status },
        setTimeout: (fn) => setTimeout(fn, 10),
        clearTimeout,
        browser: {
            permissions: {
                contains: async ({ origins }) => {
                    assert.equal(origins.join(), "https://www.youtube.com/*");
                    return granted;
                },
            },
            tabs: { query: async () => [{ id: 5, url }] },
            cookies: {
                getAllCookieStores: async () => stores,
                getAll: async (query) => { queried = query; return cookies; },
            },
            runtime: {
                onMessage: { addListener: (listener) => { receiveRequest = listener; } },
                sendMessage: (payload) => receiveRequest(payload),
                connectNative: () => {
                    nativeConnections++;
                    return {
                        disconnect() {},
                        postMessage() {},
                        onDisconnect: { addListener() {} },
                        onMessage: {
                            addListener(listener) {
                                if (challenge) {
                                    listener({ name: "watch-history-request", userInfo: { port: "4000", token, purpose } });
                                }
                            },
                        },
                    };
                },
                sendNativeMessage: async (_, payload) => { message = payload; return { received: true }; },
            },
        },
    };
    vm.runInNewContext(background, context);
    vm.runInNewContext(popup, context);
    await connect();
    return { status: status.textContent, queried, message, nativeConnections };
}

const signedIn = [
    { name: "SID", domain: ".youtube.com", value: "dummy-sid" },
    { name: "SAPISID", domain: ".youtube.com", value: "dummy-api" },
];

test("connection requires site approval and an active YouTube tab", async () => {
    for (const options of [{ granted: false }, { url: "https://www.youtube.com.example.org/" }]) {
        const result = await run(options);
        assert.equal(result.queried, undefined);
        assert.equal(result.nativeConnections, 0);
    }
});

test("sends only eligible cookies after receiving a request from Stream Tube", async () => {
    const result = await run({ cookies: [
        ...signedIn,
        { name: "SID", domain: ".google.com", value: "other-site" },
        { name: "PREF", domain: ".youtube.com", value: "unrelated" },
    ] });
    assert.equal(result.queried.url, "https://www.youtube.com/");
    assert.equal(result.queried.storeId, "test-profile");
    assert.equal(result.message.token, token);
    assert.equal(result.message.purpose, "history-and-reactions");
    assert.deepEqual(Array.from(result.message.cookies, ({ name }) => name), ["SID", "SAPISID"]);
    assert.match(result.status, /Session sent/);

    const missing = await run({ cookies: signedIn, challenge: false });
    assert.equal(missing.message, undefined);
});

test("signed-out Safari does not hand over a partial session", async () => {
    const result = await run({ cookies: [signedIn[0]] });
    assert.equal(result.nativeConnections, 0);
    assert.match(result.status, /didn’t provide the session/);
});

test("a cookie store from another Safari profile cannot supply the session", async () => {
    const result = await run({ cookies: signedIn, stores: [{ id: "another-profile", tabIds: [3] }] });
    assert.equal(result.queried, undefined);
    assert.equal(result.nativeConnections, 0);
    assert.match(result.status, /find the cookie store for this YouTube tab/);
});

test("the shared connection does not send the session to an older prototype", async () => {
    for (const purpose of [null, "watch-history", "comment-reactions", "all-account-access"]) {
        const result = await run({ cookies: signedIn, purpose });
        assert.equal(result.message, undefined);
        assert.match(result.status, /matching Stream Tube build/);
    }
});
