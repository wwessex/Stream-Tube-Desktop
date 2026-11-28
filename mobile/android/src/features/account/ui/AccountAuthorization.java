package io.github.shiroksh.streamtube;

import android.app.Activity;
import android.app.Fragment;
import android.app.PendingIntent;
import android.content.Intent;
import android.content.IntentSender;
import android.net.Uri;
import android.os.Bundle;
import com.google.android.gms.auth.api.identity.AuthorizationRequest;
import com.google.android.gms.auth.api.identity.AuthorizationResult;
import com.google.android.gms.auth.api.identity.ClearTokenRequest;
import com.google.android.gms.auth.api.identity.Identity;
import com.google.android.gms.common.ConnectionResult;
import com.google.android.gms.common.GoogleApiAvailability;
import com.google.android.gms.common.api.ApiException;
import com.google.android.gms.common.api.CommonStatusCodes;
import com.google.android.gms.common.api.Scope;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.Collections;
import java.util.concurrent.Executors;
import java.util.concurrent.ExecutorService;
import javax.net.ssl.HttpsURLConnection;
import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

public final class AccountAuthorization extends Fragment {
    private static final String TAG = "stream-tube-google-authorization";
    private static final String READ_ONLY = "https://www.googleapis.com/auth/youtube.readonly";
    private static final int LAUNCH_CONSENT = 702;
    private static final int OK = 0;
    private static final int CANCELLED = 1;
    private static final int GOOGLE_UNAVAILABLE = 2;
    private static final int AUTH_CONFIGURATION = 3;
    private static final int NETWORK_ERROR = 4;
    private static final int RECONNECT_NEEDED = 5;
    private static final int NO_YOUTUBE_CHANNEL = 6;
    private static final int RATE_LIMITED = 7;
    private static final int MAX_CHANNELS = 500;
    private static final int MAX_RESPONSE_BYTES = 512 * 1024;
    private static final ExecutorService NETWORK = Executors.newSingleThreadExecutor();
    private static String currentAccessToken;

    private int request;
    private boolean interactive;
    private boolean active;
    private boolean showingConsent;

    public static boolean available(Activity activity) {
        return GoogleApiAvailability.getInstance().isGooglePlayServicesAvailable(activity)
            == ConnectionResult.SUCCESS;
    }

    public static void authorize(Activity activity, int request, boolean interactive, boolean chooseAccount) {
        activity.runOnUiThread(() -> {
            if (activity.isDestroyed() || activity.isFinishing()) {
                deliver(request, CANCELLED, "", "[]");
                return;
            }
            if (!available(activity)) {
                deliver(request, GOOGLE_UNAVAILABLE, "", "[]");
                return;
            }
            AccountAuthorization current =
                (AccountAuthorization) activity.getFragmentManager().findFragmentByTag(TAG);
            if (current != null) {
                current.remove();
            }
            AccountAuthorization fragment = new AccountAuthorization();
            Bundle options = new Bundle();
            options.putInt("request", request);
            options.putBoolean("interactive", interactive);
            options.putBoolean("chooseAccount", chooseAccount);
            fragment.setArguments(options);
            activity.getFragmentManager().beginTransaction()
                .add(fragment, TAG).commitAllowingStateLoss();
        });
    }

    public static void disconnect(Activity activity) {
        activity.runOnUiThread(() -> {
            AccountAuthorization current =
                (AccountAuthorization) activity.getFragmentManager().findFragmentByTag(TAG);
            if (current != null) {
                current.remove();
            }
            String token = currentAccessToken;
            currentAccessToken = null;
            if (token != null && available(activity)) {
                Identity.getAuthorizationClient(activity)
                    .clearToken(ClearTokenRequest.builder().setToken(token).build());
            }
        });
    }

    @Override public void onActivityCreated(Bundle savedState) {
        super.onActivityCreated(savedState);
        Bundle options = getArguments();
        if (options == null || getActivity() == null) {
            remove();
            return;
        }
        request = options.getInt("request");
        interactive = options.getBoolean("interactive");
        active = true;
        if (savedState != null && savedState.getBoolean("showingConsent")) {
            showingConsent = true;
            return;
        }
        AuthorizationRequest.Builder authorization = AuthorizationRequest.builder()
            .setRequestedScopes(Collections.singletonList(new Scope(READ_ONLY)))
            .setOptOutIncludingGrantedScopes(true);
        if (options.getBoolean("chooseAccount")) {
            authorization.setPrompt(AuthorizationRequest.Prompt.SELECT_ACCOUNT);
        }
        Identity.getAuthorizationClient(getActivity()).authorize(authorization.build())
            .addOnSuccessListener(this::receive)
            .addOnFailureListener(error -> finish(googleStatus(error), "", "[]"));
    }

    @Override public void onSaveInstanceState(Bundle out) {
        super.onSaveInstanceState(out);
        out.putBoolean("showingConsent", showingConsent);
    }

    private void receive(AuthorizationResult result) {
        if (!active || getActivity() == null) {
            return;
        }
        if (result.hasResolution()) {
            if (!interactive) {
                finish(RECONNECT_NEEDED, "", "[]");
                return;
            }
            PendingIntent intent = result.getPendingIntent();
            if (intent == null) {
                finish(GOOGLE_UNAVAILABLE, "", "[]");
                return;
            }
            try {
                showingConsent = true;
                startIntentSenderForResult(intent.getIntentSender(), LAUNCH_CONSENT,
                    null, 0, 0, 0, null);
            } catch (IntentSender.SendIntentException failure) {
                finish(GOOGLE_UNAVAILABLE, "", "[]");
            }
            return;
        }
        String token = result.getAccessToken();
        if (token == null || token.isEmpty()) {
            finish(RECONNECT_NEEDED, "", "[]");
            return;
        }
        Activity activity = getActivity();
        NETWORK.execute(() -> {
            try {
                JSONObject profile = readGoogle(token, endpoint("channels", 1, ""));
                JSONArray identities = profile.optJSONArray("items");
                JSONObject identity = identities == null ? null : identities.optJSONObject(0);
                JSONObject snippet = identity == null ? null : identity.optJSONObject("snippet");
                if (snippet == null) {
                    activity.runOnUiThread(() -> finish(NO_YOUTUBE_CHANNEL, "", "[]"));
                    return;
                }
                String name = snippet.optString("title");
                String channels = readSubscriptions(token).toString();
                activity.runOnUiThread(() -> {
                    if (active) {
                        currentAccessToken = token;
                        finish(OK, name, channels);
                    }
                });
            } catch (GoogleHttpError failure) {
                activity.runOnUiThread(() -> finish(failure.authorizationStatus(), "", "[]"));
            } catch (IOException | JSONException failure) {
                activity.runOnUiThread(() -> finish(NETWORK_ERROR, "", "[]"));
            }
        });
    }

    @Override public void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != LAUNCH_CONSENT || !active || getActivity() == null) {
            return;
        }
        showingConsent = false;
        if (data == null) {
            finish(CANCELLED, "", "[]");
            return;
        }
        try {
            AuthorizationResult result = Identity.getAuthorizationClient(getActivity())
                .getAuthorizationResultFromIntent(data);
            if (resultCode == Activity.RESULT_OK) {
                receive(result);
            } else {
                finish(CANCELLED, "", "[]");
            }
        } catch (ApiException error) {
            finish(googleStatus(error), "", "[]");
        }
    }

    private static int googleStatus(Exception error) {
        if (error instanceof ApiException) {
            int status = ((ApiException) error).getStatusCode();
            if (status == CommonStatusCodes.DEVELOPER_ERROR) {
                return AUTH_CONFIGURATION;
            }
            if (status == CommonStatusCodes.CANCELED) {
                return CANCELLED;
            }
            if (status == CommonStatusCodes.NETWORK_ERROR) {
                return NETWORK_ERROR;
            }
            if (status == CommonStatusCodes.SIGN_IN_REQUIRED) {
                return RECONNECT_NEEDED;
            }
        }
        return GOOGLE_UNAVAILABLE;
    }

    private static JSONArray readSubscriptions(String token)
            throws IOException, JSONException {
        JSONArray channels = new JSONArray();
        String cursor = "";
        for (int page = 0; page < MAX_CHANNELS / 50; page++) {
            JSONObject packet = readGoogle(token, endpoint("subscriptions", 50, cursor));
            JSONArray subscriptions = packet.optJSONArray("items");
            if (subscriptions == null) {
                throw new IOException("Subscriptions are unavailable");
            }
            for (int index = 0; index < subscriptions.length(); index++) {
                JSONObject snippet = subscriptions.getJSONObject(index).optJSONObject("snippet");
                JSONObject resource = snippet == null ? null : snippet.optJSONObject("resourceId");
                if (resource != null && resource.optString("kind").equals("youtube#channel")) {
                    channels.put(new JSONObject()
                        .put("id", resource.optString("channelId"))
                        .put("name", snippet.optString("title")));
                }
            }
            String next = packet.optString("nextPageToken");
            if (next.isEmpty() || next.equals(cursor)) {
                break;
            }
            cursor = next;
        }
        return channels;
    }

    private static Uri endpoint(String resource, int count, String cursor) {
        Uri.Builder url = new Uri.Builder().scheme("https").authority("www.googleapis.com")
            .appendPath("youtube").appendPath("v3").appendPath(resource)
            .appendQueryParameter("mine", "true")
            .appendQueryParameter("part", "snippet")
            .appendQueryParameter("maxResults", Integer.toString(count));
        if (resource.equals("subscriptions")) {
            url.appendQueryParameter("fields", "nextPageToken,items(snippet(title,resourceId))");
        } else {
            url.appendQueryParameter("fields", "items(snippet(title))");
        }
        if (!cursor.isEmpty()) {
            url.appendQueryParameter("pageToken", cursor);
        }
        return url.build();
    }

    private static JSONObject readGoogle(String token, Uri uri) throws IOException, JSONException {
        HttpsURLConnection connection = (HttpsURLConnection) new URL(uri.toString()).openConnection();
        connection.setInstanceFollowRedirects(false);
        connection.setConnectTimeout(10000);
        connection.setReadTimeout(10000);
        connection.setRequestProperty("Authorization", "Bearer " + token);
        try {
            int status = connection.getResponseCode();
            if (status != 200) {
                throw new GoogleHttpError(status, errorReason(connection));
            }
            try (InputStream input = connection.getInputStream();
                 ByteArrayOutputStream bytes = new ByteArrayOutputStream()) {
                byte[] chunk = new byte[8192];
                for (int read; (read = input.read(chunk)) != -1; ) {
                    if (bytes.size() + read > MAX_RESPONSE_BYTES) {
                        throw new IOException("Google response is too large");
                    }
                    bytes.write(chunk, 0, read);
                }
                return new JSONObject(new String(bytes.toByteArray(), StandardCharsets.UTF_8));
            }
        } finally {
            connection.disconnect();
        }
    }

    private static String errorReason(HttpsURLConnection connection) {
        try (InputStream input = connection.getErrorStream()) {
            if (input == null) {
                return "";
            }
            byte[] body = new byte[8192];
            int length = input.read(body);
            if (length <= 0) {
                return "";
            }
            JSONObject error = new JSONObject(new String(body, 0, length, StandardCharsets.UTF_8))
                .optJSONObject("error");
            JSONArray errors = error == null ? null : error.optJSONArray("errors");
            JSONObject first = errors == null ? null : errors.optJSONObject(0);
            return first == null ? "" : first.optString("reason");
        } catch (IOException | JSONException ignored) {
            return "";
        }
    }

    private void finish(int status, String name, String channels) {
        if (!active) {
            return;
        }
        deliver(request, status, name, channels);
        remove();
    }

    private void remove() {
        active = false;
        if (getFragmentManager() != null) {
            getFragmentManager().beginTransaction().remove(this).commitAllowingStateLoss();
        }
    }

    @Override public void onDestroy() {
        active = false;
        super.onDestroy();
    }

    private static final class GoogleHttpError extends IOException {
        final int status;
        final String reason;

        GoogleHttpError(int status, String reason) {
            this.status = status;
            this.reason = reason;
        }

        int authorizationStatus() {
            if (reason.equals("youtubeSignupRequired")) {
                return NO_YOUTUBE_CHANNEL;
            }
            if (status == 429 || reason.equals("quotaExceeded") || reason.equals("dailyLimitExceeded")
                    || reason.equals("rateLimitExceeded") || reason.equals("userRateLimitExceeded")) {
                return RATE_LIMITED;
            }
            if (status == 401) {
                return RECONNECT_NEEDED;
            }
            return status == 403 ? AUTH_CONFIGURATION : NETWORK_ERROR;
        }
    }

    private static native void deliver(int request, int status, String name, String channels);
}
