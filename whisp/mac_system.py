"""macOS permissions, login item, haptics, and application location."""

import sys
from pathlib import Path


def bundle_path():
    if not getattr(sys, "frozen", False):
        return None
    return next((p for p in Path(sys.executable).resolve().parents if p.suffix == ".app"), None)


def installed_bundle():
    bundle = bundle_path()
    return (
        bundle
        if bundle and bundle.parent in (Path("/Applications"), Path.home() / "Applications")
        else None
    )


def permission_status():
    import ApplicationServices as ax
    import AVFoundation as av
    import Quartz

    return {
        "Microphone": av.AVCaptureDevice.authorizationStatusForMediaType_(av.AVMediaTypeAudio)
        == av.AVAuthorizationStatusAuthorized,
        "Accessibility": bool(ax.AXIsProcessTrusted()),
        "Input monitoring": bool(Quartz.CGPreflightListenEventAccess()),
    }


def open_settings(kind):
    from AppKit import NSWorkspace
    from Foundation import NSURL

    suffix = {
        "Microphone": "Privacy_Microphone",
        "Accessibility": "Privacy_Accessibility",
        "Input monitoring": "Privacy_ListenEvent",
    }.get(kind)
    url = (
        f"x-apple.systempreferences:com.apple.preference.security?{suffix}"
        if suffix
        else "x-apple.systempreferences:com.apple.Keyboard-Settings.extension"
    )
    NSWorkspace.sharedWorkspace().openURL_(NSURL.URLWithString_(url))


def request_permission(kind, done):
    if kind == "Microphone":
        import AVFoundation as av

        status = av.AVCaptureDevice.authorizationStatusForMediaType_(av.AVMediaTypeAudio)
        if status == av.AVAuthorizationStatusNotDetermined:
            av.AVCaptureDevice.requestAccessForMediaType_completionHandler_(
                av.AVMediaTypeAudio, lambda granted: done()
            )
        elif status != av.AVAuthorizationStatusAuthorized:
            open_settings(kind)
        return
    if kind == "Accessibility":
        import ApplicationServices as ax

        ax.AXIsProcessTrustedWithOptions({ax.kAXTrustedCheckOptionPrompt: True})
    elif kind == "Input monitoring":
        import Quartz

        Quartz.CGRequestListenEventAccess()
    open_settings(kind)


def set_login_enabled(enabled):
    from ServiceManagement import (
        SMAppService,
        SMAppServiceStatusEnabled,
        SMAppServiceStatusRequiresApproval,
    )

    if not installed_bundle():
        if enabled:
            raise RuntimeError("Move Whisp.app into Applications before enabling Open at login.")
        return
    service = SMAppService.mainAppService()
    if enabled:
        if service.status() != SMAppServiceStatusEnabled:
            ok, error = service.registerAndReturnError_(None)
            if not ok:
                raise RuntimeError(
                    "macOS could not enable login startup. Check System Settings → General → Login Items."
                )
        if service.status() == SMAppServiceStatusRequiresApproval:
            SMAppService.openSystemSettingsLoginItems()
            raise RuntimeError("Approve Whisp in Login Items to finish enabling startup.")
        if service.status() != SMAppServiceStatusEnabled:
            raise RuntimeError(
                "Login startup was not enabled. Check Login Items in System Settings."
            )
    elif service.status() != 0:
        ok, error = service.unregisterAndReturnError_(None)
        if not ok:
            raise RuntimeError(
                "Could not disable login startup. Remove Whisp in System Settings → General → Login Items."
            )


def login_enabled():
    from ServiceManagement import SMAppService, SMAppServiceStatusEnabled

    return SMAppService.mainAppService().status() == SMAppServiceStatusEnabled


def haptic(start):
    from AppKit import (
        NSHapticFeedbackManager,
        NSHapticFeedbackPatternAlignment,
        NSHapticFeedbackPatternGeneric,
        NSHapticFeedbackPerformanceTimeNow,
    )

    pattern = NSHapticFeedbackPatternAlignment if start else NSHapticFeedbackPatternGeneric
    NSHapticFeedbackManager.defaultPerformer().performFeedbackPattern_performanceTime_(
        pattern, NSHapticFeedbackPerformanceTimeNow
    )
