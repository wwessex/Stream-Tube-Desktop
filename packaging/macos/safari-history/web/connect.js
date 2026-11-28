const youtube = "https://www.youtube.com/";
const site = { origins: [youtube + "*"] };
const authCookies = new Set([
    "SID", "HSID", "SSID", "APISID", "SAPISID", "SIDCC", "LOGIN_INFO",
    "__Secure-1PSID", "__Secure-3PSID", "__Secure-1PAPISID", "__Secure-3PAPISID",
    "__Secure-1PSIDTS", "__Secure-3PSIDTS", "__Secure-1PSIDCC", "__Secure-3PSIDCC",
]);
const button = document.getElementById("connect");
const status = document.getElementById("status");

button.addEventListener("click", connect);

async function connect() {
    button.disabled = true;
    status.textContent = "Checking access to YouTube…";
    try {
        if (!await browser.permissions.contains(site)) {
            status.textContent = "Allow the extension on youtube.com from Safari’s website permissions, then try again.";
            return;
        }
        const [tab] = await browser.tabs.query({ active: true, currentWindow: true });
        if (!tab?.url?.startsWith(youtube)) {
            status.textContent = "Open a signed-in www.youtube.com tab and try again.";
            return;
        }
        // Safari may omit cookieStoreId on a tab, including in named profiles.
        const stores = await browser.cookies.getAllCookieStores();
        const store = stores.find(({ tabIds }) => tabIds.includes(tab.id));
        if (!store) {
            status.textContent = "Safari couldn’t find the cookie store for this YouTube tab. Reopen YouTube in this profile and try again.";
            return;
        }
        const cookies = (await browser.cookies.getAll({ url: youtube, storeId: store.id }))
            .filter(({ name, domain }) => authCookies.has(name)
                && [".youtube.com", "youtube.com", ".www.youtube.com", "www.youtube.com"].includes(domain))
            .map(({ name, domain, value }) => ({ name, domain, value }));
        const names = new Set(cookies.map(({ name }) => name));
        if (!(names.has("SID") || names.has("__Secure-3PSID"))
                || !(names.has("SAPISID") || names.has("__Secure-3PAPISID"))) {
            status.textContent = "Safari didn’t provide the session Stream Tube needs from this profile. Check that YouTube is signed in in this tab.";
            return;
        }
        status.textContent = "Waiting for Stream Tube…";
        const request = await browser.runtime.sendMessage({ type: "watch-history-request" });
        if (!request) {
            status.textContent = "Start a Safari connection in the matching Stream Tube build and try again.";
            return;
        }
        const reply = await browser.runtime.sendNativeMessage(
            "io.github.avaneesh-inamdar.StreamTube.SafariPrototype", { ...request, cookies }
        );
        status.textContent = reply?.received
            ? "Session sent. Return to Stream Tube to check the connection."
            : "Stream Tube did not receive the session. Restart the connection and try again.";
    } catch {
        status.textContent = "Safari couldn’t connect. Check the extension’s access to YouTube and try again.";
    } finally {
        button.disabled = false;
    }
}
