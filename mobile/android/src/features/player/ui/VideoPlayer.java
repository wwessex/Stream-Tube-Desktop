package io.github.shiroksh.streamtube;

import android.app.Activity;
import android.app.Application;
import android.app.Dialog;
import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.content.res.ColorStateList;
import android.graphics.Color;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.view.Gravity;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowInsets;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceRequest;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.SeekBar;
import android.widget.TextView;
import android.widget.Toast;
import java.text.NumberFormat;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.regex.Pattern;
import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

public final class VideoPlayer {
    private static final Pattern VIDEO_ID = Pattern.compile("[A-Za-z0-9_-]{11}");
    private static final int WATCHED = 0;
    private static final int SAVE = 1;
    private static final int CHANNEL = 2;
    private static final int CLOSED = 3;
    private static final int MORE = 4;
    private static final String CONTROLS = String.join("",
        "<style>#segments,#skip-note{position:absolute;pointer-events:none}#segments{height:3px;",
        "bottom:0;left:0;right:0}#segments span{position:absolute;height:100%;background:#e7a33e}",
        "#segments .skip{background:#36ce78}#skip-note{top:8px;right:8px;padding:5px 8px;",
        "border-radius:5px;background:#151619de;color:#fff;font:13px sans-serif}</style>",
        "<div id='segments' hidden></div><div id='skip-note' hidden aria-live='polite'></div>",
        "<script>",
        "const frame=document.getElementById('yt-player'), bar=document.getElementById('segments');",
        "const notice=document.getElementById('skip-note');",
        "let player, segments=[], skipped=new Set(), markersDrawn=false, noteTimer;",
        "window.onYouTubeIframeAPIReady=()=>{player=new YT.Player('yt-player');};",
        "window.streamTubeSetSegments=(id,list)=>{if(frame.dataset.video!==id)return;",
        "segments=list; markersDrawn=false;};",
        "window.streamTubeState=()=>{if(!player||typeof player.getCurrentTime!=='function')return null;",
        "return [player.getCurrentTime(),player.getDuration(),player.getPlayerState()];};",
        "window.streamTubeControl=(action,value)=>{const state=window.streamTubeState();if(!state)return;",
        "if(action==='play'){if(state[2]===1||state[2]===3)player.pauseVideo();else player.playVideo();return;}",
        "const time=action==='seek'?value:state[0]+value;",
        "if(Number.isFinite(time)&&state[1]>0)player.seekTo(Math.max(0,Math.min(state[1],time)),true);};",
        "setInterval(()=>{if(!player||typeof player.getCurrentTime!=='function')return;",
        "const duration=player.getDuration();if(!Number.isFinite(duration)||duration<=0)return;",
        "if(!markersDrawn){bar.textContent='';",
        "for(const s of segments){if(s.start>=duration)continue;",
        "const marker=document.createElement('span');marker.className=s.auto_skip?'skip':'';",
        "marker.style.left=(s.start/duration*100)+'%';",
        "marker.style.width=Math.max(0.15,(Math.min(s.end,duration)-s.start)/duration*100)+'%';",
        "bar.appendChild(marker);}bar.hidden=!segments.length;markersDrawn=true;}",
        "if(player.getPlayerState()!==1)return;const now=player.getCurrentTime();",
        "const index=segments.findIndex((s,i)=>s.auto_skip&&!skipped.has(i)&&",
        "s.end<duration&&now>=s.start&&now<s.end-0.15);if(index<0)return;",
        "skipped.add(index);player.seekTo(segments[index].end,true);",
        "notice.textContent='SponsorBlock';notice.hidden=false;clearTimeout(noteTimer);",
        "noteTimer=setTimeout(()=>{notice.hidden=true;},2200);",
        "console.info('Stream Tube SponsorBlock skipped');},220);",
        "</script><script src='https://www.youtube.com/iframe_api'></script>"
    );
    private static Player active;

    private VideoPlayer() {}

    public static void open(Activity activity, int session, String items, int first,
                            String language, String messages, boolean shorts, boolean more) {
        activity.runOnUiThread(() -> {
            if (activity.isFinishing() || activity.isDestroyed()) {
                event(session, CLOSED, "");
                return;
            }
            JSONObject labels = new JSONObject();
            try {
                labels = new JSONObject(messages);
                List<Track> queue = decode(items);
                if (first < 0 || first >= queue.size()) {
                    event(session, CLOSED, "");
                    return;
                }
                if (active != null) {
                    active.dialog.dismiss();
                }
                active = new Player(activity, session, queue, first, language, labels, shorts, more);
                active.show();
            } catch (RuntimeException | JSONException failure) {
                event(session, CLOSED, "");
                Toast.makeText(activity, labels.optString("Player unavailable", "Player unavailable"),
                    Toast.LENGTH_SHORT).show();
            }
        });
    }

    public static void append(Activity activity, int session, String items, boolean more, boolean failed) {
        activity.runOnUiThread(() -> {
            Player player = active;
            if (player == null || session != player.session) {
                return;
            }
            try {
                player.append(decode(items), more, failed);
            } catch (JSONException failure) {
                player.append(new ArrayList<>(), more, true);
            }
        });
    }

    public static void updateSaved(Activity activity, int session, String id, boolean saved) {
        activity.runOnUiThread(() -> {
            if (active != null && session == active.session) {
                for (Track track : active.queue) {
                    if (track.id.equals(id)) {
                        track.saved = saved;
                        break;
                    }
                }
                active.updateButtons();
            }
        });
    }

    public static void describe(Activity activity, int session, String id, String description) {
        activity.runOnUiThread(() -> {
            if (active != null && session == active.session && active.track().id.equals(id)) {
                active.description.setText(description);
                active.description.setVisibility(description.isEmpty() ? View.GONE : View.VISIBLE);
            }
        });
    }

    public static void setExtras(Activity activity, int session, String id, String packet) {
        activity.runOnUiThread(() -> {
            Player player = active;
            if (player == null || session != player.session || !player.track().id.equals(id)) {
                return;
            }
            try {
                player.extras(new JSONObject(packet));
            } catch (JSONException invalid) {
                // The video still plays without metadata from optional services.
            }
        });
    }

    private static List<Track> decode(String items) throws JSONException {
        JSONArray decoded = new JSONArray(items);
        List<Track> tracks = new ArrayList<>(decoded.length());
        for (int i = 0; i < decoded.length(); i++) {
            Track track = new Track(decoded.getJSONObject(i));
            if (!VIDEO_ID.matcher(track.id).matches()) {
                throw new JSONException("Invalid video identifier");
            }
            tracks.add(track);
        }
        return tracks;
    }

    private static final class Track {
        final String id;
        final String title;
        final String channel;
        final boolean channelLink;
        boolean saved;

        Track(JSONObject item) throws JSONException {
            id = item.getString("id");
            title = item.getString("title");
            channel = item.getString("channel");
            channelLink = item.getBoolean("channel_link");
            saved = item.getBoolean("saved");
        }
    }

    private static final class Player implements Application.ActivityLifecycleCallbacks {
        final Activity activity;
        final int session;
        final List<Track> queue;
        final String language;
        final JSONObject labels;
        final boolean shorts;
        final FrameLayout frame;
        final LinearLayout screen;
        final LinearLayout toolbar;
        final Dialog dialog;
        final WebView web;
        final PlayerChrome chrome;
        final TextView title;
        final TextView description;
        final TextView dislikes;
        final Button channel;
        final Button save;
        final Button previous;
        final Button next;
        final ScrollView details;
        final Transport transport;
        final int compactVideoHeight;
        int index;
        boolean more;
        boolean loading;
        JSONArray segments;
        boolean expanded;
        int compactUi;

        Player(Activity activity, int session, List<Track> queue, int first,
               String language, JSONObject labels, boolean shorts, boolean more) {
            this.activity = activity;
            this.session = session;
            this.queue = queue;
            this.index = first;
            this.language = language;
            this.labels = labels;
            this.shorts = shorts;
            this.more = more;

            frame = new FrameLayout(activity);
            frame.setBackgroundColor(Color.BLACK);
            screen = new LinearLayout(activity);
            screen.setOrientation(LinearLayout.VERTICAL);
            screen.setBackgroundColor(0xFF151619);
            frame.addView(screen, new FrameLayout.LayoutParams(-1, -1));
            frame.setOnApplyWindowInsetsListener((view, insets) -> {
                if (Build.VERSION.SDK_INT >= 30) {
                    android.graphics.Insets bars = insets.getInsets(
                        WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
                    screen.setPadding(bars.left, bars.top, bars.right, bars.bottom);
                } else {
                    screen.setPadding(insets.getSystemWindowInsetLeft(), insets.getSystemWindowInsetTop(),
                        insets.getSystemWindowInsetRight(), insets.getSystemWindowInsetBottom());
                }
                return insets;
            });

            chrome = new PlayerChrome(frame, screen);
            dialog = new Dialog(activity, android.R.style.Theme_Material_NoActionBar) {
                @Override public void onBackPressed() {
                    if (!chrome.hideFullscreen() && !exitExpanded()) {
                        super.onBackPressed();
                    }
                }
            };

            toolbar = new LinearLayout(activity);
            toolbar.setPadding(dp(activity, 12), 0, dp(activity, 12), 0);
            toolbar.setGravity(Gravity.CENTER_VERTICAL);
            Button close = action(activity, "‹");
            close.setTextSize(27);
            close.setContentDescription(text("Back"));
            close.setOnClickListener(view -> dialog.dismiss());
            toolbar.addView(close, new LinearLayout.LayoutParams(dp(activity, 48), dp(activity, 44)));
            TextView label = label(shorts ? "Shorts" : text("Video"), 19, Color.WHITE);
            label.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
            label.setPadding(dp(activity, 14), 0, 0, 0);
            label.setGravity(Gravity.CENTER_VERTICAL);
            toolbar.addView(label, new LinearLayout.LayoutParams(0, -1, 1));
            screen.addView(toolbar, new LinearLayout.LayoutParams(-1, dp(activity, 54)));

            web = new WebView(activity);
            setupWebView(web, this::applySegments);
            web.setWebChromeClient(chrome);
            int width = activity.getResources().getDisplayMetrics().widthPixels;
            int height = activity.getResources().getDisplayMetrics().heightPixels;
            compactVideoHeight = Math.min(width * 9 / 16, height / 2);
            if (shorts) {
                ShortsGesture pager = new ShortsGesture(activity, this::move);
                pager.addView(web, new FrameLayout.LayoutParams(-1, -1));
                screen.addView(pager, new LinearLayout.LayoutParams(-1, 0, 1));
            } else {
                screen.addView(web, new LinearLayout.LayoutParams(-1, compactVideoHeight));
            }
            transport = shorts ? null : new Transport(this);
            if (transport != null) {
                screen.addView(transport, new LinearLayout.LayoutParams(-1, -2));
            }

            details = new ScrollView(activity);
            details.setFillViewport(false);
            LinearLayout metadata = new LinearLayout(activity);
            metadata.setOrientation(LinearLayout.VERTICAL);
            metadata.setPadding(dp(activity, 14), dp(activity, 10), dp(activity, 14), dp(activity, 8));
            details.addView(metadata, new ScrollView.LayoutParams(-1, -2));
            screen.addView(details, shorts
                ? new LinearLayout.LayoutParams(-1, -2)
                : new LinearLayout.LayoutParams(-1, 0, 1));
            title = label("", 18, Color.WHITE);
            title.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
            title.setMaxLines(shorts ? 2 : 4);
            title.setEllipsize(android.text.TextUtils.TruncateAt.END);
            metadata.addView(title, new LinearLayout.LayoutParams(-1, -2));

            channel = action(activity, "");
            channel.setOnClickListener(view -> {
                event(session, CHANNEL, track().id);
                dialog.dismiss();
            });
            LinearLayout.LayoutParams channelParams = new LinearLayout.LayoutParams(-2, dp(activity, 44));
            channelParams.topMargin = dp(activity, 8);
            metadata.addView(channel, channelParams);

            LinearLayout tools = new LinearLayout(activity);
            LinearLayout.LayoutParams toolsParams = new LinearLayout.LayoutParams(-1, dp(activity, 44));
            toolsParams.topMargin = dp(activity, 8);
            metadata.addView(tools, toolsParams);
            save = tool(tools, text("Save"), () -> event(session, SAVE, track().id));
            if (!shorts) {
                tool(tools, text("Share"), this::share);
            }
            Button youtube = tool(tools, "YouTube", this::openInYouTube);
            youtube.setContentDescription(text("Open in YouTube"));
            dislikes = label("", 13, 0xFFBDBBC7);
            dislikes.setPadding(0, dp(activity, 8), 0, 0);
            metadata.addView(dislikes, new LinearLayout.LayoutParams(-1, -2));
            description = label("", 15, 0xFFDEDBE3);
            description.setTextIsSelectable(true);
            LinearLayout.LayoutParams descriptionParams = new LinearLayout.LayoutParams(-1, -2);
            descriptionParams.topMargin = dp(activity, 14);
            metadata.addView(description, descriptionParams);

            LinearLayout navigation = new LinearLayout(activity);
            previous = tool(navigation, shorts ? "↑" : "⏮", () -> move(-1));
            next = tool(navigation, shorts ? "↓" : "⏭", () -> move(1));
            previous.setContentDescription(text("Previous video"));
            next.setContentDescription(text("Next video"));
            toolbar.addView(navigation, new LinearLayout.LayoutParams(dp(activity, 104), dp(activity, 44)));

            dialog.setContentView(frame);
            dialog.setOnDismissListener(closed -> {
                if (transport != null) {
                    transport.stop();
                }
                activity.getApplication().unregisterActivityLifecycleCallbacks(this);
                chrome.hideFullscreen();
                web.stopLoading();
                if (web.getParent() instanceof ViewGroup) {
                    ((ViewGroup) web.getParent()).removeView(web);
                }
                web.destroy();
                if (active == this) {
                    active = null;
                }
                event(session, CLOSED, "");
            });
        }

        void show() {
            dialog.show();
            chrome.window = dialog.getWindow();
            activity.getApplication().registerActivityLifecycleCallbacks(this);
            watch();
            if (transport != null) {
                transport.start();
            }
        }

        Track track() {
            return queue.get(index);
        }

        private void watch() {
            Track track = track();
            title.setText(track.title);
            title.setVisibility(shorts ? View.GONE : View.VISIBLE);
            if (shorts) {
                web.setContentDescription(track.title);
            }
            channel.setText(track.channel);
            channel.setVisibility(track.channel.isEmpty() ? View.GONE : View.VISIBLE);
            channel.setEnabled(track.channelLink);
            description.setVisibility(View.GONE);
            dislikes.setVisibility(View.GONE);
            segments = null;
            if (transport != null) {
                transport.reset();
            }
            updateButtons();
            details.scrollTo(0, 0);
            String origin = "https://" + activity.getPackageName();
            String html = "<!doctype html><html><head><meta name='viewport' "
                + "content='width=device-width,initial-scale=1'><meta name='referrer' "
                + "content='strict-origin-when-cross-origin'><style>html,body{margin:0;width:100%;height:100%;"
                + "background:#000}iframe{width:100%;height:100%;border:0}</style></head><body>"
                + "<iframe id='yt-player' data-video='" + track.id
                + "' title='YouTube' src='https://www.youtube.com/embed/" + track.id
                + "?playsinline=1&amp;autoplay=1&amp;hl=" + Uri.encode(language)
                + "&amp;enablejsapi=1&amp;origin=" + Uri.encode(origin)
                + "' referrerpolicy='strict-origin-when-cross-origin'"
                + " allow='autoplay; fullscreen; encrypted-media; picture-in-picture' allowfullscreen>"
                + "</iframe>" + CONTROLS + "</body></html>";
            web.loadDataWithBaseURL(origin, html, "text/html", "UTF-8", null);
            event(session, WATCHED, track.id);
        }

        void extras(JSONObject packet) {
            if (!shorts && !packet.isNull("title")) {
                title.setText(packet.optString("title", track().title));
            }
            if (!shorts && !packet.isNull("dislikes")) {
                NumberFormat number = NumberFormat.getIntegerInstance(Locale.forLanguageTag(language));
                dislikes.setText(text("Dislikes") + " " + number.format(packet.optLong("dislikes")));
                dislikes.setVisibility(View.VISIBLE);
            }
            segments = packet.optJSONArray("segments");
            applySegments();
        }

        private void applySegments() {
            if (active == this && segments != null) {
                web.evaluateJavascript("window.streamTubeSetSegments && window.streamTubeSetSegments("
                    + JSONObject.quote(track().id) + "," + segments.toString() + ")", null);
            }
        }

        void updateButtons() {
            save.setText(track().saved ? text("Saved") : text("Save"));
            previous.setEnabled(index > 0);
            next.setEnabled(!loading && (index + 1 < queue.size() || more));
            previous.setAlpha(previous.isEnabled() ? 1f : 0.45f);
            next.setAlpha(next.isEnabled() ? 1f : 0.45f);
            next.setText(loading ? "…" : shorts ? "↓" : "⏭");
        }

        void command(String name, double value) {
            web.evaluateJavascript("window.streamTubeControl && window.streamTubeControl("
                + JSONObject.quote(name) + "," + value + ")", null);
        }

        private boolean exitExpanded() {
            if (!expanded) {
                return false;
            }
            setExpanded(false);
            return true;
        }

        void setExpanded(boolean value) {
            expanded = value;
            toolbar.setVisibility(value ? View.GONE : View.VISIBLE);
            details.setVisibility(value ? View.GONE : View.VISIBLE);
            web.setLayoutParams(new LinearLayout.LayoutParams(-1, value ? 0 : compactVideoHeight, value ? 1 : 0));
            Window window = dialog.getWindow();
            if (window != null) {
                View decor = window.getDecorView();
                if (value) {
                    compactUi = decor.getSystemUiVisibility();
                    decor.setSystemUiVisibility(compactUi | View.SYSTEM_UI_FLAG_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY);
                } else {
                    decor.setSystemUiVisibility(compactUi);
                }
                frame.requestApplyInsets();
            }
            transport.updateExpanded(value);
        }

        void move(int direction) {
            int destination = index + direction;
            if (destination >= 0 && destination < queue.size()) {
                index = destination;
                watch();
            } else if (destination == queue.size() && more && !loading) {
                loading = true;
                updateButtons();
                event(session, MORE, track().id);
            }
        }

        void append(List<Track> tracks, boolean hasMore, boolean failed) {
            boolean advance = loading && index + 1 == queue.size();
            queue.addAll(tracks);
            more = hasMore;
            loading = false;
            if (advance && !tracks.isEmpty()) {
                move(1);
            } else {
                updateButtons();
            }
            if (failed) {
                Toast.makeText(activity, text("Could not load more videos"),
                    Toast.LENGTH_SHORT).show();
            }
        }

        private String text(String source) {
            return labels.optString(source, source);
        }

        private TextView label(String text, int size, int color) {
            TextView view = new TextView(activity);
            view.setText(text);
            view.setTextSize(size);
            view.setTextColor(color);
            return view;
        }

        private Button tool(LinearLayout row, String label, Runnable action) {
            Button button = action(activity, label);
            LinearLayout.LayoutParams params = new LinearLayout.LayoutParams(0, -1, 1);
            if (row.getChildCount() > 0) {
                params.setMarginStart(dp(activity, 6));
            }
            row.addView(button, params);
            button.setOnClickListener(view -> action.run());
            return button;
        }

        private String watchUrl() {
            return "https://www.youtube.com/watch?v=" + track().id;
        }

        private void share() {
            Intent share = new Intent(Intent.ACTION_SEND).setType("text/plain")
                .putExtra(Intent.EXTRA_TEXT, watchUrl());
            launch(Intent.createChooser(share, text("Share video")));
        }

        private void openInYouTube() {
            launch(new Intent(Intent.ACTION_VIEW, Uri.parse(watchUrl())));
        }

        private void launch(Intent intent) {
            try {
                activity.startActivity(intent);
            } catch (ActivityNotFoundException unavailable) {
                Toast.makeText(activity, text("No app available"),
                    Toast.LENGTH_SHORT).show();
            }
        }

        @Override public void onActivityPaused(Activity host) {
            if (host == activity) {
                web.onPause();
                if (transport != null) {
                    transport.stop();
                }
            }
        }

        @Override public void onActivityResumed(Activity host) {
            if (host == activity) {
                web.onResume();
                if (transport != null) {
                    transport.start();
                }
            }
        }

        @Override public void onActivityDestroyed(Activity host) {
            if (host == activity) {
                dialog.dismiss();
            }
        }

        @Override public void onActivityCreated(Activity host, Bundle saved) {}
        @Override public void onActivityStarted(Activity host) {}
        @Override public void onActivityStopped(Activity host) {}
        @Override public void onActivitySaveInstanceState(Activity host, Bundle out) {}
    }

    private static final class Transport extends LinearLayout {
        private final Player player;
        private final SeekBar timeline;
        private final TextView time;
        private final Button play;
        private final Button expand;
        private final Handler handler = new Handler(Looper.getMainLooper());
        private final Runnable pollClock = this::poll;
        private double duration;
        private boolean dragging;

        Transport(Player player) {
            super(player.activity);
            this.player = player;
            Activity activity = player.activity;
            setOrientation(VERTICAL);
            setPadding(dp(activity, 12), dp(activity, 4), dp(activity, 12), dp(activity, 5));
            setBackgroundColor(0xFF151619);

            LinearLayout clock = new LinearLayout(activity);
            clock.setGravity(Gravity.CENTER_VERTICAL);
            timeline = new SeekBar(activity);
            timeline.setMax(1000);
            ColorStateList accent = ColorStateList.valueOf(0xFFF36B82);
            timeline.setProgressTintList(accent);
            timeline.setThumbTintList(accent);
            timeline.setContentDescription(player.text("Seek"));
            clock.addView(timeline, new LinearLayout.LayoutParams(0, dp(activity, 36), 1));
            time = player.label("", 12, 0xFFD1CDD5);
            time.setGravity(Gravity.END | Gravity.CENTER_VERTICAL);
            clock.addView(time, new LinearLayout.LayoutParams(-2, dp(activity, 36)));
            addView(clock, new LinearLayout.LayoutParams(-1, -2));

            LinearLayout buttons = new LinearLayout(activity);
            Button backward = player.tool(buttons, "−10", () -> player.command("relative", -10));
            backward.setContentDescription(player.text("Back 10 seconds"));
            play = player.tool(buttons, "▶", () -> player.command("play", 0));
            Button forward = player.tool(buttons, "+10", () -> player.command("relative", 10));
            forward.setContentDescription(player.text("Forward 10 seconds"));
            expand = player.tool(buttons, "⛶", () -> player.setExpanded(!player.expanded));
            updateExpanded(false);
            addView(buttons, new LinearLayout.LayoutParams(-1, dp(activity, 44)));

            timeline.setOnSeekBarChangeListener(new SeekBar.OnSeekBarChangeListener() {
                @Override public void onStartTrackingTouch(SeekBar bar) { dragging = true; }
                @Override public void onStopTrackingTouch(SeekBar bar) {
                    dragging = false;
                    if (duration > 0) {
                        player.command("seek", duration * bar.getProgress() / bar.getMax());
                    }
                }
                @Override public void onProgressChanged(SeekBar bar, int progress, boolean user) {
                    if (user) {
                        showTime(duration * progress / bar.getMax());
                    }
                }
            });
            reset();
        }

        void start() {
            stop();
            handler.post(pollClock);
        }

        void stop() {
            handler.removeCallbacks(pollClock);
        }

        void reset() {
            duration = 0;
            dragging = false;
            timeline.setEnabled(false);
            timeline.setProgress(0);
            showTime(0);
            updatePlaying(false);
        }

        void updateExpanded(boolean expanded) {
            expand.setText(expanded ? "↙" : "⛶");
            expand.setContentDescription(player.text(expanded ? "Exit fullscreen" : "Fullscreen"));
        }

        private void poll() {
            if (active != player || !player.dialog.isShowing()) {
                return;
            }
            String id = player.track().id;
            player.web.evaluateJavascript("window.streamTubeState && window.streamTubeState()", value -> {
                if (active == player && player.track().id.equals(id)) {
                    readState(value);
                }
            });
            handler.postDelayed(pollClock, 500);
        }

        private void readState(String value) {
            if (value == null || !value.startsWith("[")) {
                return;
            }
            try {
                JSONArray state = new JSONArray(value);
                duration = state.optDouble(1, 0);
                if (!Double.isFinite(duration) || duration <= 0) {
                    return;
                }
                timeline.setEnabled(true);
                if (!dragging) {
                    double current = state.optDouble(0, 0);
                    timeline.setProgress((int) Math.round(1000 * Math.min(1, Math.max(0, current / duration))));
                    showTime(current);
                }
                int playback = state.optInt(2);
                updatePlaying(playback == 1 || playback == 3);
            } catch (JSONException invalid) {
                // A frame can finish loading between polls.
            }
        }

        private void updatePlaying(boolean playing) {
            play.setText(playing ? "Ⅱ" : "▶");
            play.setContentDescription(player.text(playing ? "Pause" : "Play"));
        }

        private void showTime(double current) {
            time.setText(formatTime((long) current) + " / " + formatTime((long) duration));
        }

        private static String formatTime(long elapsed) {
            long seconds = elapsed % 60;
            if (elapsed < 3600) {
                return String.format(Locale.ROOT, "%d:%02d", elapsed / 60, seconds);
            }
            return String.format(Locale.ROOT, "%d:%02d:%02d", elapsed / 3600, elapsed / 60 % 60, seconds);
        }
    }

    private static final class ShortsGesture extends FrameLayout {
        interface Move {
            void by(int direction);
        }

        private final Move move;
        private final int threshold;
        private float x;
        private float y;
        private boolean swiping;
        private boolean multiTouch;

        ShortsGesture(Activity activity, Move move) {
            super(activity);
            this.move = move;
            threshold = dp(activity, 42);
        }

        @Override public boolean dispatchTouchEvent(MotionEvent event) {
            int action = event.getActionMasked();
            if (action == MotionEvent.ACTION_DOWN) {
                x = event.getX();
                y = event.getY();
                swiping = false;
                multiTouch = false;
            } else if (action == MotionEvent.ACTION_POINTER_DOWN) {
                multiTouch = true;
            }

            if (!swiping && !multiTouch && action == MotionEvent.ACTION_MOVE && isSwipe(event)) {
                MotionEvent cancelled = MotionEvent.obtain(event);
                cancelled.setAction(MotionEvent.ACTION_CANCEL);
                super.dispatchTouchEvent(cancelled);
                cancelled.recycle();
                swiping = true;
            }
            if (!swiping) {
                return super.dispatchTouchEvent(event);
            }
            if (action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) {
                swiping = false;
                if (!multiTouch && action == MotionEvent.ACTION_UP && isSwipe(event)) {
                    move.by(event.getY() < y ? 1 : -1);
                }
            }
            return true;
        }

        private boolean isSwipe(MotionEvent event) {
            float dy = Math.abs(event.getY() - y);
            return dy > threshold && dy > Math.abs(event.getX() - x) * 1.3f;
        }
    }

    private static void setupWebView(WebView web, Runnable documentReady) {
        web.setBackgroundColor(Color.BLACK);
        WebSettings settings = web.getSettings();
        settings.setJavaScriptEnabled(true);
        settings.setDomStorageEnabled(true);
        settings.setMediaPlaybackRequiresUserGesture(false);
        settings.setAllowFileAccess(false);
        settings.setAllowContentAccess(false);
        settings.setMixedContentMode(WebSettings.MIXED_CONTENT_NEVER_ALLOW);
        web.setWebViewClient(new WebViewClient() {
            @Override public void onPageFinished(WebView view, String url) {
                documentReady.run();
            }

            @Override public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
                Uri uri = request.getUrl();
                if (request.isForMainFrame() || !"https".equals(uri.getScheme())) {
                    return true;
                }
                String host = uri.getHost();
                return host == null || !(host.equals("youtube.com") || host.endsWith(".youtube.com"));
            }
        });
    }

    private static Button action(Activity activity, String title) {
        Button button = new Button(activity);
        button.setText(title);
        button.setAllCaps(false);
        button.setTextColor(Color.WHITE);
        button.setTextSize(13);
        button.setPadding(dp(activity, 7), 0, dp(activity, 7), 0);
        GradientDrawable fill = new GradientDrawable();
        fill.setColor(0xFF272831);
        fill.setCornerRadius(dp(activity, 9));
        button.setBackground(fill);
        return button;
    }

    private static int dp(Activity activity, int value) {
        return Math.round(activity.getResources().getDisplayMetrics().density * value);
    }

    private static final class PlayerChrome extends WebChromeClient {
        private final FrameLayout frame;
        private final View screen;
        private View fullscreen;
        private Window window;
        private int normalChrome;
        private CustomViewCallback fullscreenCallback;

        PlayerChrome(FrameLayout frame, View screen) {
            this.frame = frame;
            this.screen = screen;
        }

        @Override public void onShowCustomView(View view, CustomViewCallback callback) {
            if (fullscreen != null) {
                callback.onCustomViewHidden();
                return;
            }
            if (view.getParent() instanceof ViewGroup) {
                ((ViewGroup) view.getParent()).removeView(view);
            }
            fullscreen = view;
            fullscreenCallback = callback;
            screen.setVisibility(View.GONE);
            if (window != null) {
                View decor = window.getDecorView();
                normalChrome = decor.getSystemUiVisibility();
                decor.setSystemUiVisibility(normalChrome | View.SYSTEM_UI_FLAG_FULLSCREEN
                    | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY);
            }
            frame.addView(view, new FrameLayout.LayoutParams(-1, -1));
        }

        @Override public void onHideCustomView() {
            hideFullscreen();
        }

        boolean hideFullscreen() {
            if (fullscreen == null) {
                return false;
            }
            View view = fullscreen;
            CustomViewCallback callback = fullscreenCallback;
            fullscreen = null;
            fullscreenCallback = null;
            frame.removeView(view);
            screen.setVisibility(View.VISIBLE);
            if (window != null) {
                window.getDecorView().setSystemUiVisibility(normalChrome);
            }
            callback.onCustomViewHidden();
            return true;
        }
    }

    private static native void event(int session, int kind, String id);
}
