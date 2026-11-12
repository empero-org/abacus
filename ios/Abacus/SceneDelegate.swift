import UIKit

class SceneDelegate: UIResponder, UIWindowSceneDelegate {

    var window: UIWindow?

    func scene(
        _ scene: UIScene,
        willConnectTo session: UISceneSession,
        options connectionOptions: UIScene.ConnectionOptions
    ) {
        guard let windowScene = (scene as? UIWindowScene) else { return }

        let window = UIWindow(windowScene: windowScene)
        let viewController = ViewController()
        window.rootViewController = viewController
        window.backgroundColor = UIColor(red: 14/255.0, green: 11/255.0, blue: 20/255.0, alpha: 1.0)
        self.window = window
        window.makeKeyAndVisible()

        if let urlContext = connectionOptions.urlContexts.first {
            viewController.handleDeepLink(urlContext.url)
        }
    }

    func scene(_ scene: UIScene, openURLContexts URLContexts: Set<UIOpenURLContext>) {
        if let url = URLContexts.first?.url,
           let rootVC = window?.rootViewController as? ViewController {
            rootVC.handleDeepLink(url)
        }
    }
}
