package ai.empero.abacus;

import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Typeface;
import android.net.Uri;
import android.os.Bundle;
import android.text.InputType;
import android.text.SpannableString;
import android.text.Spanned;
import android.text.style.ForegroundColorSpan;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowManager;
import android.view.inputmethod.InputMethodManager;
import android.webkit.CookieManager;
import android.webkit.JavascriptInterface;
import android.webkit.ValueCallback;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.Button;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONObject;

import java.io.InputStream;
import java.net.URI;
import java.util.Locale;

/**
 * Abacus Mobile for Empero (abacus.empero.org + local terminal bridge).
 *
 * Serves our full Abacus Desktop-styled mobile UI from assets/app.html directly
 * under the https://abacus.empero.org/__abacus_mobile__ origin so that Abacus Sync
 * REST (/web/...) and WebSocket (/v1/remote/browser/...) work natively with
 * first-party session cookies and zero CORS friction.
 */
public class MainActivity extends Activity {
    static final String INK = "#0E0B14";
    static final String PANEL = "#151020";
    static final String LINE = "#2A2338";
    static final String TEXT = "#E9E4F0";
    static final String SUBTLE = "#BCB5C9";
    static final String MUTED = "#8F88A0";
    static final String ACCENT = "#B66BFF";
    static final String DANGER = "#FF8F9B";
    static final String SYNC_HOST = "abacus.empero.org";
    static final String SYNC_APP_URL = "https://abacus.empero.org/__abacus_mobile__";
    static final int FILE_CHOOSER_REQ = 1001;

    SharedPreferences prefs;
    FrameLayout root;
    WebView web;
    View connectScroll;
    LinearLayout connect;
    ValueCallback<Uri[]> fileCallback;
    String mode = "sync";
    String scheme = "https";
    String host = SYNC_HOST;
    String port = "443";
    String token = "";
    String pairToken = "";
    String sessionId = "";
    String page = SYNC_APP_URL;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        prefs = getSharedPreferences("abacus", Context.MODE_PRIVATE);
        Window window = getWindow();
        window.addFlags(WindowManager.LayoutParams.FLAG_DRAWS_SYSTEM_BAR_BACKGROUNDS);
        window.setStatusBarColor(Color.parseColor(INK));
        window.setNavigationBarColor(Color.parseColor(INK));

        root = new FrameLayout(this);
        root.setBackgroundColor(Color.parseColor(INK));
        setContentView(root);

        String opened = extractIntentUrl(getIntent());
        if (!opened.isEmpty() && openLink(opened)) {
            prefs.edit().putString("link", mode.equals("sync") ? SYNC_APP_URL : opened).apply();
            showWeb();
            return;
        }
        String saved = prefs.getString("link", "");
        if (saved.isEmpty()) {
            openLink("sync");
            prefs.edit().putString("link", SYNC_APP_URL).apply();
            showWeb();
        } else if (openLink(saved)) {
            showWeb();
        } else {
            showConnect("That link did not parse.");
        }
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        if (intent != null) {
            String approveCode = intent.getStringExtra("approve_code");
            if (approveCode != null && !approveCode.trim().isEmpty() && web != null) {
                String clean = approveCode.trim().replace("'", "");
                web.evaluateJavascript(
                        "window.__abacusApproveDevice && window.__abacusApproveDevice('" + clean + "')",
                        null);
                return;
            }
        }
        String opened = extractIntentUrl(intent);
        if (!opened.isEmpty() && openLink(opened)) {
            prefs.edit().putString("link", mode.equals("sync") ? SYNC_APP_URL : opened).apply();
            showWeb();
        }
    }

    private String extractIntentUrl(Intent intent) {
        if (intent == null) {
            return "";
        }
        if (intent.getData() != null) {
            return intent.getData().toString();
        }
        String extra = intent.getStringExtra("url");
        return extra != null ? extra.trim() : "";
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == FILE_CHOOSER_REQ && fileCallback != null) {
            Uri[] results = null;
            if (resultCode == Activity.RESULT_OK && data != null && data.getData() != null) {
                results = new Uri[]{data.getData()};
            }
            fileCallback.onReceiveValue(results);
            fileCallback = null;
        }
    }

    /**
     * Parses either:
     * 1. An Abacus Sync link (`https://abacus.empero.org/...`, `/pair#t=...`, `/remote/...`, or bare pairing token)
     *    -> opens our native mobile UI under `SYNC_APP_URL` in `"sync"` mode.
     * 2. A direct `--mobile` terminal link (`http://host:port/?t=token`)
     *    -> opens in `"local"` mode.
     */
    private boolean openLink(String raw) {
        String text = raw == null ? "" : raw.trim();
        if (text.isEmpty() || text.equalsIgnoreCase("sync")) {
            mode = "sync";
            scheme = "https";
            host = SYNC_HOST;
            port = "443";
            token = "";
            pairToken = "";
            sessionId = "";
            page = SYNC_APP_URL;
            return true;
        }
        // Bare pairing token from `abacus sync pair` or `/remote qr`
        if (!text.contains("://") && !text.contains(".") && !text.contains(":") && text.length() >= 20) {
            mode = "sync";
            scheme = "https";
            host = SYNC_HOST;
            port = "443";
            token = "";
            pairToken = text;
            sessionId = "";
            page = SYNC_APP_URL;
            return true;
        }
        if (!text.contains("://")) {
            text = "http://" + text;
        }
        try {
            URI uri = URI.create(text);
            String parsedHost = uri.getHost();
            if (parsedHost == null) {
                return false;
            }
            if (SYNC_HOST.equalsIgnoreCase(parsedHost)) {
                mode = "sync";
                scheme = "https";
                host = SYNC_HOST;
                port = "443";
                token = "";
                pairToken = extractSyncPairToken(uri);
                sessionId = extractSyncSessionId(uri.getPath());
                page = SYNC_APP_URL;
                return true;
            }
            boolean ours = uri.getRawQuery() != null
                    && (uri.getPath() == null || uri.getPath().isEmpty() || uri.getPath().equals("/"));
            if (ours && parse(text)) {
                mode = "local";
                pairToken = "";
                sessionId = "";
                page = "";
                return true;
            }
            String kind = uri.getScheme() == null ? "" : uri.getScheme().toLowerCase(Locale.ROOT);
            if (!kind.equals("https") && !kind.equals("http")) {
                return false;
            }
            mode = "local";
            page = text;
            return true;
        } catch (RuntimeException error) {
            return false;
        }
    }

    private String extractSyncPairToken(URI uri) {
        String frag = uri.getRawFragment();
        if (frag != null) {
            for (String part : frag.split("&")) {
                if (part.startsWith("t=") && part.length() > 2) {
                    return Uri.decode(part.substring(2));
                }
            }
        }
        String query = uri.getRawQuery();
        if (query != null) {
            for (String part : query.split("&")) {
                if (part.startsWith("t=") && part.length() > 2) {
                    return Uri.decode(part.substring(2));
                }
            }
        }
        return "";
    }

    String extractSyncSessionId(String path) {
        if (path == null) {
            return "";
        }
        String[] parts = path.split("/");
        for (int i = 0; i < parts.length - 1; i++) {
            if ("remote".equals(parts[i]) || "sessions".equals(parts[i])) {
                String candidate = parts[i + 1];
                if (candidate.length() == 36) {
                    return candidate;
                }
            }
        }
        return "";
    }

    // ------------------------------------------------------------------ link

    /** `http://host:port/?t=token` (or a bare `host:port`) into the three parts. */
    private boolean parse(String raw) {
        String text = raw.trim();
        if (text.isEmpty()) {
            return false;
        }
        if (!text.contains("://")) {
            text = "http://" + text;
        }
        try {
            URI uri = URI.create(text);
            String parsedHost = uri.getHost();
            String parsedScheme = "https".equalsIgnoreCase(uri.getScheme()) ? "https" : "http";
            int parsedPort = uri.getPort() > 0 ? uri.getPort() : ("https".equals(parsedScheme) ? 443 : 8787);
            if (parsedHost == null) {
                return false;
            }
            String query = uri.getRawQuery();
            String parsedToken = "";
            if (query != null) {
                for (String pair : query.split("&")) {
                    int at = pair.indexOf('=');
                    if (at > 0 && pair.substring(0, at).equals("t")) {
                        parsedToken = pair.substring(at + 1);
                    }
                }
            }
            if (parsedToken.isEmpty()) {
                return false;
            }
            scheme = parsedScheme;
            host = parsedHost;
            port = String.valueOf(parsedPort);
            token = parsedToken;
            return true;
        } catch (RuntimeException error) {
            return false;
        }
    }

    // ------------------------------------------------------------------- web

    private void showWeb() {
        if (web == null) {
            WebView.setWebContentsDebuggingEnabled(true);
            web = new WebView(this);
            web.setBackgroundColor(Color.parseColor(INK));
            WebSettings settings = web.getSettings();
            settings.setJavaScriptEnabled(true);
            settings.setDomStorageEnabled(true);
            settings.setSupportZoom(false);
            settings.setBuiltInZoomControls(false);
            settings.setTextZoom(100);
            settings.setMediaPlaybackRequiresUserGesture(false);
            CookieManager cookieManager = CookieManager.getInstance();
            cookieManager.setAcceptCookie(true);
            cookieManager.setAcceptThirdPartyCookies(web, true);
            web.setWebViewClient(new AppWebClient());
            web.setWebChromeClient(new ChromeClient());
            web.addJavascriptInterface(this, "AbacusApp");
            root.addView(web, new FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        }
        web.setVisibility(View.VISIBLE);
        if (connectScroll != null) {
            connectScroll.clearFocus();
            connectScroll.setVisibility(View.GONE);
        }
        hideKeyboard();
        web.requestFocus();
        if ("sync".equals(mode)) {
            loadSyncApp();
        } else {
            String targetUrl = page.isEmpty() ? base() + "/?t=" + token : page;
            web.loadUrl(targetUrl);
        }
    }

    void loadSyncApp() {
        if (web == null) {
            return;
        }
        try {
            InputStream in = getAssets().open("app.html");
            java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            in.close();
            String html = out.toString("UTF-8");
            web.loadDataWithBaseURL(SYNC_APP_URL, html, "text/html", "UTF-8", SYNC_APP_URL);
        } catch (Exception ignored) {
            web.loadUrl(SYNC_APP_URL);
        }
    }

    private void hideKeyboard() {
        try {
            InputMethodManager imm = (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
            if (imm != null && root != null) {
                imm.hideSoftInputFromWindow(root.getWindowToken(), 0);
            }
        } catch (RuntimeException ignored) {
        }
    }

    private void showConnect(String problem) {
        if (web != null) {
            web.setVisibility(View.GONE);
        }
        if (connect == null) {
            connect = buildConnect();
            android.widget.ScrollView scroll = new android.widget.ScrollView(this);
            scroll.setBackgroundColor(Color.parseColor(INK));
            scroll.setFillViewport(true);
            scroll.addView(connect);
            connectScroll = scroll;
            root.addView(scroll, new FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        }
        connectScroll.setVisibility(View.VISIBLE);
        connect.setVisibility(View.VISIBLE);
        TextView note = connect.findViewWithTag("note");
        if (note != null) {
            note.setText(problem == null || problem.isEmpty() ? "" : problem);
            note.setVisibility(problem == null || problem.isEmpty() ? View.GONE : View.VISIBLE);
        }
        EditText field = connect.findViewWithTag("link");
        if (field != null) {
            String saved = prefs.getString("link", "");
            if (SYNC_APP_URL.equals(saved)) {
                saved = "";
            }
            if (saved.isEmpty()) {
                String clip = clipboard();
                if (!clip.isEmpty() && (clip.contains("abacus.empero.org") || clip.contains("?t="))) {
                    saved = clip.trim();
                }
            }
            field.setText(saved);
            field.setSelection(field.getText().length());
        }
    }

    final class AppWebClient extends WebViewClient {
        AppWebClient() {
        }

        @Override
        public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
            if (request != null && request.getUrl() != null) {
                Uri uri = request.getUrl();
                String reqHost = uri.getHost();
                String reqPath = uri.getPath() != null ? uri.getPath() : "";
                boolean isMobileAppPath = reqPath.equals("/__abacus_mobile__")
                        || reqPath.equals("/__abacus_mobile__/")
                        || reqPath.equals("/sessions")
                        || reqPath.equals("/sessions/")
                        || reqPath.startsWith("/remote");
                if (SYNC_HOST.equalsIgnoreCase(reqHost) && isMobileAppPath) {
                    String sid = extractSyncSessionId(reqPath);
                    if (!sid.isEmpty()) {
                        sessionId = sid;
                    }
                    mode = "sync";
                    loadSyncApp();
                    return true;
                }
            }
            return false;
        }

        @Override
        public WebResourceResponse shouldInterceptRequest(WebView view, WebResourceRequest request) {
            if (request != null && request.getUrl() != null) {
                Uri uri = request.getUrl();
                String reqHost = uri.getHost();
                String reqPath = uri.getPath() != null ? uri.getPath() : "";
                boolean isMobileAppPath = reqPath.equals("/__abacus_mobile__")
                        || reqPath.equals("/__abacus_mobile__/")
                        || (request.isForMainFrame()
                        && (reqPath.equals("/sessions") || reqPath.equals("/sessions/") || reqPath.startsWith("/remote")));
                if (SYNC_HOST.equalsIgnoreCase(reqHost) && isMobileAppPath) {
                    String sid = extractSyncSessionId(reqPath);
                    if (!sid.isEmpty()) {
                        sessionId = sid;
                    }
                    mode = "sync";
                    try {
                        InputStream stream = getAssets().open("app.html");
                        return new WebResourceResponse("text/html", "UTF-8", stream);
                    } catch (Exception ignored) {
                    }
                }
            }
            return super.shouldInterceptRequest(view, request);
        }

        @Override
        public void onPageFinished(WebView view, String url) {
            super.onPageFinished(view, url);
            try {
                CookieManager.getInstance().flush();
            } catch (RuntimeException ignored) {
            }
            if (url != null && url.startsWith("https://abacus.empero.org/") && !url.contains("/__abacus_mobile__")) {
                Uri uri = Uri.parse(url);
                String path = uri.getPath() != null ? uri.getPath() : "";
                if (path.equals("/sessions") || path.startsWith("/remote") || path.equals("/")) {
                    String sid = extractSyncSessionId(path);
                    if (!sid.isEmpty()) {
                        sessionId = sid;
                    }
                    mode = "sync";
                    loadSyncApp();
                }
            }
        }
    }

    final class ChromeClient extends WebChromeClient {
        ChromeClient() {
        }

        @Override
        public boolean onShowFileChooser(WebView webView, ValueCallback<Uri[]> filePathCallback, FileChooserParams fileChooserParams) {
            if (fileCallback != null) {
                fileCallback.onReceiveValue(null);
            }
            fileCallback = filePathCallback;
            try {
                Intent intent = new Intent(Intent.ACTION_GET_CONTENT);
                intent.addCategory(Intent.CATEGORY_OPENABLE);
                intent.setType("image/*");
                startActivityForResult(Intent.createChooser(intent, "Choose background image"), FILE_CHOOSER_REQ);
                return true;
            } catch (Exception e) {
                fileCallback = null;
                return false;
            }
        }
    }

    final class BrandMarkView extends View {
        private final Paint framePaint = new Paint(Paint.ANTI_ALIAS_FLAG);
        private final Paint gridPaint = new Paint(Paint.ANTI_ALIAS_FLAG);

        BrandMarkView(Context context) {
            super(context);
            framePaint.setStyle(Paint.Style.STROKE);
            framePaint.setColor(Color.parseColor(TEXT));
            framePaint.setStrokeWidth(dp(1.6f));

            gridPaint.setStyle(Paint.Style.STROKE);
            gridPaint.setColor(Color.parseColor(ACCENT));
            gridPaint.setStrokeWidth(dp(1.6f));
        }

        @Override
        protected void onDraw(Canvas canvas) {
            super.onDraw(canvas);
            float w = getWidth();
            float h = getHeight();
            float pad = dp(1.5f);
            canvas.drawRect(pad, pad, w - pad, h - pad, framePaint);

            float x1 = w * 0.22f;
            float x2 = w * 0.78f;
            canvas.drawLine(x1, h * 0.28f, x2, h * 0.28f, gridPaint);
            canvas.drawLine(x1, h * 0.50f, x2, h * 0.50f, gridPaint);
            canvas.drawLine(x1, h * 0.72f, x2, h * 0.72f, gridPaint);

            float y1 = h * 0.16f;
            float y2 = h * 0.84f;
            canvas.drawLine(w * 0.36f, y1, w * 0.36f, y2, gridPaint);
            canvas.drawLine(w * 0.64f, y1, w * 0.64f, y2, gridPaint);
        }
    }

    private View buildBrandMark() {
        return new BrandMarkView(this);
    }

    private LinearLayout buildConnect() {
        LinearLayout box = new LinearLayout(this);
        box.setOrientation(LinearLayout.VERTICAL);
        box.setGravity(Gravity.CENTER_VERTICAL);
        box.setBackgroundColor(Color.parseColor(INK));
        int pad = dp(24);
        box.setPadding(pad, pad, pad, pad);

        LinearLayout brandRow = new LinearLayout(this);
        brandRow.setOrientation(LinearLayout.HORIZONTAL);
        brandRow.setGravity(Gravity.CENTER_VERTICAL);
        LinearLayout.LayoutParams brandRowParams = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        brandRowParams.bottomMargin = dp(18);

        View logo = buildBrandMark();
        LinearLayout.LayoutParams logoParams = new LinearLayout.LayoutParams(dp(22), dp(22));
        logoParams.rightMargin = dp(10);
        brandRow.addView(logo, logoParams);

        TextView wordmark = new TextView(this);
        wordmark.setText("abacus");
        wordmark.setTextColor(Color.parseColor(TEXT));
        wordmark.setTextSize(TypedValue.COMPLEX_UNIT_SP, 16);
        wordmark.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        LinearLayout.LayoutParams wmParams = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        wmParams.rightMargin = dp(8);
        brandRow.addView(wordmark, wmParams);

        TextView badge = new TextView(this);
        badge.setText("SYNC · MOBILE");
        badge.setTextColor(Color.parseColor(ACCENT));
        badge.setTextSize(TypedValue.COMPLEX_UNIT_SP, 10);
        badge.setTypeface(Typeface.MONOSPACE, Typeface.BOLD);
        badge.setPadding(dp(7), dp(2), dp(7), dp(2));
        badge.setBackground(rounded("#1A1228", "#5A3880", 99));
        brandRow.addView(badge);

        box.addView(brandRow, brandRowParams);

        TextView hero = new TextView(this);
        String heroFull = "What are we building today?";
        SpannableString heroSpan = new SpannableString(heroFull);
        int todayIdx = heroFull.indexOf("today?");
        if (todayIdx >= 0) {
            heroSpan.setSpan(new ForegroundColorSpan(Color.parseColor(ACCENT)),
                    todayIdx, heroFull.length(), Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
        }
        hero.setText(heroSpan);
        hero.setTextColor(Color.parseColor(TEXT));
        hero.setTextSize(TypedValue.COMPLEX_UNIT_SP, 28);
        hero.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        box.addView(hero);

        TextView lead = new TextView(this);
        lead.setText("Connect through Abacus Sync (abacus.empero.org) to manage your laptop sessions from anywhere, or paste a pairing / local terminal link.");
        lead.setTextColor(Color.parseColor(MUTED));
        lead.setTextSize(TypedValue.COMPLEX_UNIT_SP, 14);
        lead.setLineSpacing(0f, 1.25f);
        lead.setPadding(0, dp(8), 0, dp(20));
        box.addView(lead);

        Button syncBtn = new Button(this);
        syncBtn.setText("Open Abacus Sync (abacus.empero.org)");
        syncBtn.setAllCaps(false);
        syncBtn.setTextColor(Color.parseColor(INK));
        syncBtn.setTextSize(TypedValue.COMPLEX_UNIT_SP, 15);
        syncBtn.setTypeface(Typeface.DEFAULT_BOLD);
        syncBtn.setBackground(rounded(ACCENT, ACCENT, 14));
        LinearLayout.LayoutParams syncParams = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, dp(48));
        syncParams.bottomMargin = dp(18);
        syncBtn.setOnClickListener(view -> {
            openLink("sync");
            prefs.edit().putString("link", SYNC_APP_URL).apply();
            showWeb();
        });
        box.addView(syncBtn, syncParams);

        TextView label = new TextView(this);
        label.setText("PAIRING OR LOCAL SESSION LINK");
        label.setTextColor(Color.parseColor(MUTED));
        label.setTextSize(TypedValue.COMPLEX_UNIT_SP, 11);
        label.setTypeface(Typeface.MONOSPACE, Typeface.BOLD);
        label.setPadding(dp(2), 0, 0, dp(6));
        box.addView(label);

        EditText field = new EditText(this);
        field.setTag("link");
        field.setHint("https://abacus.empero.org/pair#t=… or http://host:8787/?t=…");
        field.setHintTextColor(Color.parseColor("#6F6879"));
        field.setTextColor(Color.parseColor(TEXT));
        field.setTextSize(TypedValue.COMPLEX_UNIT_SP, 14);
        field.setTypeface(Typeface.MONOSPACE);
        field.setSingleLine(false);
        field.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
        field.setBackground(rounded(PANEL, LINE, 14));
        field.setPadding(dp(14), dp(13), dp(14), dp(13));
        box.addView(field, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));

        TextView note = new TextView(this);
        note.setTag("note");
        note.setTextColor(Color.parseColor(DANGER));
        note.setTextSize(TypedValue.COMPLEX_UNIT_SP, 13);
        note.setPadding(0, dp(10), 0, 0);
        note.setVisibility(View.GONE);
        box.addView(note);

        Button go = new Button(this);
        go.setText("Connect with link / token");
        go.setAllCaps(false);
        go.setTextColor(Color.parseColor(TEXT));
        go.setTextSize(TypedValue.COMPLEX_UNIT_SP, 14);
        go.setTypeface(Typeface.DEFAULT_BOLD);
        go.setBackground(rounded("#1B1529", "#4B3270", 14));
        LinearLayout.LayoutParams goParams = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, dp(46));
        goParams.topMargin = dp(12);
        go.setOnClickListener(view -> {
            String raw = field.getText().toString().trim();
            if (raw.isEmpty()) {
                openLink("sync");
                prefs.edit().putString("link", SYNC_APP_URL).apply();
                showWeb();
                return;
            }
            if (!openLink(raw)) {
                TextView problem = connect.findViewWithTag("note");
                problem.setText("That is neither an abacus.empero.org link nor a terminal link (?t=…).");
                problem.setVisibility(View.VISIBLE);
                return;
            }
            prefs.edit().putString("link", "sync".equals(mode) ? SYNC_APP_URL : raw).apply();
            showWeb();
        });
        box.addView(go, goParams);

        Button paste = new Button(this);
        paste.setText("Paste from clipboard");
        paste.setAllCaps(false);
        paste.setTextColor(Color.parseColor(SUBTLE));
        paste.setTextSize(TypedValue.COMPLEX_UNIT_SP, 14);
        paste.setBackground(rounded(PANEL, LINE, 14));
        LinearLayout.LayoutParams pasteParams = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, dp(46));
        pasteParams.topMargin = dp(10);
        paste.setOnClickListener(view -> {
            String clip = clipboard();
            if (clip.isEmpty()) {
                TextView problem = connect.findViewWithTag("note");
                problem.setText("The clipboard is empty.");
                problem.setVisibility(View.VISIBLE);
                return;
            }
            field.setText(clip);
            field.setSelection(field.getText().length());
        });
        box.addView(paste, pasteParams);

        TextView hint = new TextView(this);
        hint.setText("Run `abacus` on your laptop and use `/remote qr` or `abacus sync pair` to pair with abacus.empero.org.");
        hint.setTextColor(Color.parseColor(MUTED));
        hint.setTextSize(TypedValue.COMPLEX_UNIT_SP, 12);
        hint.setPadding(0, dp(18), 0, 0);
        box.addView(hint);
        return box;
    }

    private String clipboard() {
        try {
            android.content.ClipboardManager manager =
                    (android.content.ClipboardManager) getSystemService(CLIPBOARD_SERVICE);
            if (manager == null || !manager.hasPrimaryClip()) {
                return "";
            }
            CharSequence text = manager.getPrimaryClip().getItemAt(0).coerceToText(this);
            return text == null ? "" : text.toString().trim();
        } catch (RuntimeException error) {
            return "";
        }
    }

    private String base() {
        boolean standard = ("https".equals(scheme) && "443".equals(port)) || ("http".equals(scheme) && "80".equals(port));
        return scheme + "://" + host + (standard ? "" : ":" + port);
    }

    private android.graphics.drawable.Drawable rounded(String fill, String stroke, int radius) {
        android.graphics.drawable.GradientDrawable shape = new android.graphics.drawable.GradientDrawable();
        shape.setColor(Color.parseColor(fill));
        shape.setCornerRadius(dp(radius));
        shape.setStroke(dp(1), Color.parseColor(stroke));
        return shape;
    }

    int dp(float value) {
        return Math.round(value * getResources().getDisplayMetrics().density);
    }

    // ---------------------------------------------------------------- bridge

    @JavascriptInterface
    public String target() {
        try {
            JSONObject object = new JSONObject();
            object.put("mode", mode);
            object.put("host", host);
            object.put("port", port);
            object.put("scheme", scheme);
            object.put("token", token);
            object.put("pairToken", pairToken);
            object.put("sessionId", sessionId);
            return object.toString();
        } catch (Exception error) {
            return "{}";
        }
    }

    @JavascriptInterface
    public void clearPairToken() {
        pairToken = "";
        prefs.edit().putString("link", SYNC_APP_URL).apply();
    }

    @JavascriptInterface
    public void changeLink() {
        runOnUiThread(() -> showConnect(""));
    }

    @JavascriptInterface
    public void toast(String text) {
        runOnUiThread(() -> Toast.makeText(this, text, Toast.LENGTH_SHORT).show());
    }

    // ------------------------------------------------------------------ back

    @Override
    public void onBackPressed() {
        if (web != null && web.getVisibility() == View.VISIBLE) {
            web.evaluateJavascript(
                    "(function(){var s=document.querySelector('.sheet-wrap.open');" +
                            "if(s){s.classList.remove('open');return 'closed';}return 'none';})()",
                    value -> {
                        if (value == null || !value.contains("closed")) {
                            moveTaskToBack(true);
                        }
                    });
            return;
        }
        super.onBackPressed();
    }
}
