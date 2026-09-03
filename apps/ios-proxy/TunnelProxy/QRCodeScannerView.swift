@preconcurrency import AVFoundation
import SwiftUI
import UIKit

struct QRCodeScannerView: UIViewControllerRepresentable {
    var onCode: (String) -> Void
    var onCancel: () -> Void
    var onManualImport: () -> Void

    func makeUIViewController(context: Context) -> QRCodeScannerViewController {
        QRCodeScannerViewController(
            onCode: onCode,
            onCancel: onCancel,
            onManualImport: onManualImport
        )
    }

    func updateUIViewController(_ uiViewController: QRCodeScannerViewController, context: Context) {
    }
}

final class QRCodeScannerViewController: UIViewController, @preconcurrency AVCaptureMetadataOutputObjectsDelegate {
    private let onCode: (String) -> Void
    private let onCancel: () -> Void
    private let onManualImport: () -> Void
    private let session = AVCaptureSession()
    private let sessionQueue = DispatchQueue(label: "com.buhuipao.tunnelproxy.qr.session")
    private var previewLayer: AVCaptureVideoPreviewLayer?
    private var didComplete = false

    init(
        onCode: @escaping (String) -> Void,
        onCancel: @escaping () -> Void,
        onManualImport: @escaping () -> Void
    ) {
        self.onCode = onCode
        self.onCancel = onCancel
        self.onManualImport = onManualImport
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        addControls()
        configureCameraAccess()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        previewLayer?.frame = view.bounds
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        stopSession()
    }

    func metadataOutput(
        _ output: AVCaptureMetadataOutput,
        didOutput metadataObjects: [AVMetadataObject],
        from connection: AVCaptureConnection
    ) {
        guard !didComplete,
              let object = metadataObjects.first as? AVMetadataMachineReadableCodeObject,
              object.type == .qr,
              let value = object.stringValue,
              !value.isEmpty
        else {
            return
        }

        didComplete = true
        stopSession()
        DispatchQueue.main.async { [onCode] in
            onCode(value)
        }
    }

    private func configureCameraAccess() {
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            configureSession()
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .video) { [weak self] granted in
                DispatchQueue.main.async {
                    granted ? self?.configureSession() : self?.showUnavailableState()
                }
            }
        case .denied, .restricted:
            showUnavailableState()
        @unknown default:
            showUnavailableState()
        }
    }

    private func configureSession() {
        guard let device = AVCaptureDevice.default(for: .video) else {
            showUnavailableState()
            return
        }

        do {
            let input = try AVCaptureDeviceInput(device: device)
            let output = AVCaptureMetadataOutput()

            guard session.canAddInput(input), session.canAddOutput(output) else {
                showUnavailableState()
                return
            }

            session.beginConfiguration()
            session.addInput(input)
            session.addOutput(output)
            output.setMetadataObjectsDelegate(self, queue: sessionQueue)
            output.metadataObjectTypes = [.qr]
            session.commitConfiguration()

            let layer = AVCaptureVideoPreviewLayer(session: session)
            layer.videoGravity = .resizeAspectFill
            layer.frame = view.bounds
            view.layer.insertSublayer(layer, at: 0)
            previewLayer = layer
            startSession()
        } catch {
            showUnavailableState()
        }
    }

    private func startSession() {
        sessionQueue.async { [session] in
            guard !session.isRunning else {
                return
            }
            session.startRunning()
        }
    }

    private func stopSession() {
        sessionQueue.async { [session] in
            guard session.isRunning else {
                return
            }
            session.stopRunning()
        }
    }

    private func addControls() {
        let cancelButton = makeControlButton(title: "Cancel", action: #selector(cancelTapped))
        let pasteButton = makeControlButton(title: "Use a file instead", action: #selector(pasteTapped))

        let stack = UIStackView(arrangedSubviews: [cancelButton, pasteButton])
        stack.axis = .horizontal
        stack.spacing = 12
        stack.distribution = .equalSpacing
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)

        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -16),
            stack.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 16),
        ])
    }

    private func makeControlButton(title: String, action: Selector) -> UIButton {
        var configuration = UIButton.Configuration.filled()
        configuration.title = title
        // The scanner draws over the live camera feed, which is dark whatever
        // the app theme is, so this control keeps a translucent light fill.
        configuration.baseBackgroundColor = UIColor(white: 1.0, alpha: 0.18)
        configuration.baseForegroundColor = .white
        configuration.cornerStyle = .medium

        let button = UIButton(configuration: configuration)
        button.addTarget(self, action: action, for: .touchUpInside)
        return button
    }

    private func showUnavailableState() {
        let label = UILabel()
        label.text = "Camera unavailable"
        label.textColor = .white
        label.font = .preferredFont(forTextStyle: .title3)
        label.textAlignment = .center
        label.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(label)

        NSLayoutConstraint.activate([
            label.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            label.centerYAnchor.constraint(equalTo: view.centerYAnchor),
        ])
    }

    @objc private func cancelTapped() {
        stopSession()
        onCancel()
    }

    @objc private func pasteTapped() {
        stopSession()
        onManualImport()
    }
}
