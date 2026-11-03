package ai.empero.abacus;

import android.app.Activity;
import android.content.Context;
import android.content.SharedPreferences;
import android.graphics.Color;
import android.graphics.Typeface;
import android.os.Bundle;
import android.text.InputType;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowManager;
import android.webkit.JavascriptInterface;
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

import java.net.URI;

/**
 * Abacus on a phone: the terminal's phone page, in an app.
 *
 * The page itself ships in assets, so the UI opens instantly and works with
 * no network; what it needs is the agent's link — host, port and the token
 * the terminal printed. That is the one thing this activity owns: a small
 * connect screen, remembered between launches, handed to the page through a
 * JavaScript bridge.
 */
public class MainActivity extends Activity {
    private static final String INK = "#0a0b0d";
    private static final String PANEL = "#14171c";
    private static final String LINE = "#23262e";
    private static final String TEXT = "#e9ebef";
    private static final String MUTED = "#9aa3af";
    private static final String MINT = "#7dd3a0";

    private SharedPreferences prefs;
    private FrameLayout root;
    private WebView web;
    private LinearLayout connect;
    private String host = "";
    private String port = "";
    private String token = "";

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

        // A link opened from anywhere (`am start -d …`, a QR reader) wins over
        // the remembered one: it is what the person just asked for.
        if (getIntent() != null && getIntent().getData() != null) {
            String opened = getIntent().getData().toString();
            if (parse(opened)) {
                prefs.edit().putString("link", opened).apply();
                showWeb();
                return;
            }
        }
        String saved = prefs.getString("link", "");
        if (saved.isEmpty()) {
            showConnect("");
        } else if (parse(saved)) {
            showWeb();
        } else {
            showConnect("That link did not parse.");
        }
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
            int parsedPort = uri.getPort() > 0 ? uri.getPort() : 8787;
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
            web = new WebView(this);
            web.setBackgroundColor(Color.parseColor(INK));
            WebSettings settings = web.getSettings();
            settings.setJavaScriptEnabled(true);
            settings.setDomStorageEnabled(true);
            settings.setSupportZoom(false);
            settings.setBuiltInZoomControls(false);
            settings.setTextZoom(100);
            settings.setMediaPlaybackRequiresUserGesture(false);
            web.setWebViewClient(new WebViewClient());
            web.addJavascriptInterface(this, "AbacusApp");
            root.addView(web, new FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        }
        web.setVisibility(View.VISIBLE);
        if (connect != null) {
            connect.setVisibility(View.GONE);
        }
        web.loadUrl("http://" + host + ":" + port + "/?t=" + token);
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
            root.addView(scroll, new FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        }
        connect.setVisibility(View.VISIBLE);
        TextView note = connect.findViewWithTag("note");
        if (note != null) {
            note.setText(problem == null || problem.isEmpty() ? "" : problem);
            note.setVisibility(problem == null || problem.isEmpty() ? View.GONE : View.VISIBLE);
        }
        EditText field = connect.findViewWithTag("link");
        if (field != null) {
            String saved = prefs.getString("link", "");
            if (saved.isEmpty()) {
                // First run: the link is almost certainly still on the
                // clipboard, so offer it rather than making them hunt for it.
                String clip = clipboard();
                if (parse(clip)) {
                    saved = clip;
                }
            }
            field.setText(saved);
            field.setSelection(field.getText().length());
            field.requestFocus();
        }
    }

    private LinearLayout buildConnect() {
        LinearLayout box = new LinearLayout(this);
        box.setOrientation(LinearLayout.VERTICAL);
        box.setGravity(Gravity.CENTER_VERTICAL);
        box.setBackgroundColor(Color.parseColor(INK));
        int pad = dp(26);
        box.setPadding(pad, pad, pad, pad);

        TextView mark = new TextView(this);
        mark.setText("Abacus");
        mark.setTextColor(Color.parseColor(MINT));
        mark.setTextSize(TypedValue.COMPLEX_UNIT_SP, 30);
        mark.setTypeface(Typeface.create("sans-serif-light", Typeface.NORMAL));
        box.addView(mark);

        TextView lead = new TextView(this);
        lead.setText("Run /mobile in the terminal, then paste the link it prints.");
        lead.setTextColor(Color.parseColor(MUTED));
        lead.setTextSize(TypedValue.COMPLEX_UNIT_SP, 14);
        lead.setPadding(0, dp(8), 0, dp(22));
        box.addView(lead);

        EditText field = new EditText(this);
        field.setTag("link");
        field.setHint("http://192.168.1.10:8787/?t=…");
        field.setHintTextColor(Color.parseColor("#7c838f"));
        field.setTextColor(Color.parseColor(TEXT));
        field.setTextSize(TypedValue.COMPLEX_UNIT_SP, 14);
        field.setSingleLine(false);
        field.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
        field.setBackground(rounded(PANEL, LINE, 14));
        field.setPadding(dp(14), dp(12), dp(14), dp(12));
        box.addView(field, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));

        TextView note = new TextView(this);
        note.setTag("note");
        note.setTextColor(Color.parseColor("#f2706b"));
        note.setTextSize(TypedValue.COMPLEX_UNIT_SP, 13);
        note.setPadding(0, dp(10), 0, 0);
        note.setVisibility(View.GONE);
        box.addView(note);

        Button go = new Button(this);
        go.setText("Connect");
        go.setAllCaps(false);
        go.setTextColor(Color.parseColor("#06251a"));
        go.setTextSize(TypedValue.COMPLEX_UNIT_SP, 16);
        go.setTypeface(Typeface.DEFAULT_BOLD);
        go.setBackground(rounded(MINT, MINT, 14));
        LinearLayout.LayoutParams goParams = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, dp(50));
        goParams.topMargin = dp(16);
        go.setOnClickListener(view -> {
            String raw = field.getText().toString();
            if (!parse(raw)) {
                TextView problem = connect.findViewWithTag("note");
                problem.setText("Paste the whole link, including ?t=…");
                problem.setVisibility(View.VISIBLE);
                return;
            }
            prefs.edit().putString("link", raw.trim()).apply();
            showWeb();
        });
        box.addView(go, goParams);

        Button paste = new Button(this);
        paste.setText("Paste from clipboard");
        paste.setAllCaps(false);
        paste.setTextColor(Color.parseColor(TEXT));
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
        hint.setText("The link is the only credential: it controls this session.");
        hint.setTextColor(Color.parseColor("#8a919c"));
        hint.setTextSize(TypedValue.COMPLEX_UNIT_SP, 12);
        hint.setPadding(0, dp(18), 0, 0);
        box.addView(hint);
        return box;
    }

    /** The clipboard's text when it holds something link-shaped, else "". */
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

    private android.graphics.drawable.Drawable rounded(String fill, String stroke, int radius) {
        android.graphics.drawable.GradientDrawable shape = new android.graphics.drawable.GradientDrawable();
        shape.setColor(Color.parseColor(fill));
        shape.setCornerRadius(dp(radius));
        shape.setStroke(dp(1), Color.parseColor(stroke));
        return shape;
    }

    private int dp(int value) {
        return Math.round(value * getResources().getDisplayMetrics().density);
    }

    // ---------------------------------------------------------------- bridge

    /** What the page asks the app for (the JavaScript interface object). */
    @JavascriptInterface
    public String target() {
            try {
                JSONObject object = new JSONObject();
                object.put("host", host);
                object.put("port", port);
                object.put("token", token);
                return object.toString();
            } catch (Exception error) {
            return "{}";
        }
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
