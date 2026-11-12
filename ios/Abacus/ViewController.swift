import UIKit
import WebKit

class ViewController: UIViewController, WKNavigationDelegate, WKUIDelegate {

    private var webView: WKWebView!
    private let syncAppURL = URL(string: "https://abacus.empero.org/__abacus_mobile__")!
    private let backgroundColor = UIColor(red: 14/255.0, green: 11/255.0, blue: 20/255.0, alpha: 1.0)

    override func viewDidLoad() {
        super.viewDidLoad()
        setupUI()
        loadApp()
    }

    override var preferredStatusBarStyle: UIStatusBarStyle {
        return .lightContent
    }

    private func setupUI() {
        view.backgroundColor = backgroundColor

        let configuration = WKWebViewConfiguration()
        configuration.allowsInlineMediaPlayback = true
        configuration.mediaTypesRequiringUserActionForPlayback = []
        configuration.preferences.setValue(true, forKey: "allowFileAccessFromFileURLs")

        webView = WKWebView(frame: .zero, configuration: configuration)
        webView.translatesAutoresizingMaskIntoConstraints = false
        webView.navigationDelegate = self
        webView.uiDelegate = self
        webView.backgroundColor = backgroundColor
        webView.isOpaque = false
        webView.scrollView.backgroundColor = backgroundColor
        webView.scrollView.contentInsetAdjustmentBehavior = .never

        if #available(iOS 16.4, *) {
            webView.isInspectable = true
        }

        view.addSubview(webView)

        NSLayoutConstraint.activate([
            webView.topAnchor.constraint(equalTo: view.topAnchor),
            webView.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            webView.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            webView.trailingAnchor.constraint(equalTo: view.trailingAnchor)
        ])
    }

    private func loadApp() {
        if let htmlPath = Bundle.main.path(forResource: "app", ofType: "html") {
            let fileURL = URL(fileURLWithPath: htmlPath)
            do {
                let htmlString = try String(contentsOf: fileURL, encoding: .utf8)
                webView.loadHTMLString(htmlString, baseURL: syncAppURL)
                return
            } catch {
                print("Failed to read bundled app.html: \(error)")
            }
        }
        // Fallback to live URL
        webView.load(URLRequest(url: syncAppURL))
    }

    func handleDeepLink(_ url: URL) {
        let urlString = url.absoluteString
        let js = "window.__abacusOpenLink && window.__abacusOpenLink('\(urlString)');"
        webView.evaluateJavaScript(js, completionHandler: nil)
    }

    // MARK: - WKUIDelegate

    func webView(
        _ webView: WKWebView,
        runJavaScriptAlertPanelWithMessage message: String,
        initiatedByFrame frame: WKFrameInfo,
        completionHandler: @escaping () -> Void
    ) {
        let alert = UIAlertController(title: "Abacus", message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in completionHandler() })
        present(alert, animated: true)
    }

    func webView(
        _ webView: WKWebView,
        runJavaScriptConfirmPanelWithMessage message: String,
        initiatedByFrame frame: WKFrameInfo,
        completionHandler: @escaping (Bool) -> Void
    ) {
        let alert = UIAlertController(title: "Abacus", message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel) { _ in completionHandler(false) })
        alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in completionHandler(true) })
        present(alert, animated: true)
    }
}
