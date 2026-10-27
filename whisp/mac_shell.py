"""Menu-bar controls and a non-activating recording pill, on the Cocoa main thread."""

import objc
from AppKit import (
    NSApplication,
    NSApplicationActivationPolicyAccessory,
    NSBackingStoreBuffered,
    NSColor,
    NSFloatingWindowLevel,
    NSFont,
    NSImage,
    NSMenu,
    NSMenuItem,
    NSObject,
    NSPanel,
    NSScreen,
    NSStatusBar,
    NSTextField,
    NSVariableStatusItemLength,
    NSWindowCollectionBehaviorCanJoinAllSpaces,
    NSWindowCollectionBehaviorFullScreenAuxiliary,
    NSWindowStyleMaskBorderless,
    NSWindowStyleMaskNonactivatingPanel,
)


class MenuActions(NSObject):
    @objc.IBAction
    def settings_(self, sender):
        self.owner.show_settings()

    @objc.IBAction
    def record_(self, sender):
        self.owner.toggle()

    @objc.IBAction
    def cancel_(self, sender):
        self.owner.controller.cancel()

    @objc.IBAction
    def quit_(self, sender):
        self.owner.quit()


class MacShell:
    def __init__(self, owner):
        self.owner = owner
        NSApplication.sharedApplication().setActivationPolicy_(
            NSApplicationActivationPolicyAccessory
        )
        self.actions = MenuActions.alloc().init()
        self.actions.owner = owner
        self.item = NSStatusBar.systemStatusBar().statusItemWithLength_(NSVariableStatusItemLength)
        image = NSImage.imageWithSystemSymbolName_accessibilityDescription_(
            "waveform", "Whisp dictation"
        )
        if image:
            image.setTemplate_(True)
            self.item.button().setImage_(image)
        else:
            self.item.button().setTitle_("Whisp")
        menu = NSMenu.alloc().init()
        menu.setAutoenablesItems_(False)
        self.status = NSMenuItem.alloc().initWithTitle_action_keyEquivalent_(
            "Whisp · Setting up", None, ""
        )
        self.status.setEnabled_(False)
        menu.addItem_(self.status)
        menu.addItem_(NSMenuItem.separatorItem())
        self.record_item = self._menu_item(menu, "Start recording", "record:")
        self.cancel_item = self._menu_item(menu, "Cancel recording", "cancel:")
        self._menu_item(menu, "Settings…", "settings:")
        menu.addItem_(NSMenuItem.separatorItem())
        self._menu_item(menu, "Quit Whisp", "quit:", "q")
        self.item.setMenu_(menu)
        self.panel = NSPanel.alloc().initWithContentRect_styleMask_backing_defer_(
            ((0, 0), (290, 48)),
            NSWindowStyleMaskBorderless | NSWindowStyleMaskNonactivatingPanel,
            NSBackingStoreBuffered,
            False,
        )
        self.panel.setLevel_(NSFloatingWindowLevel)
        self.panel.setHidesOnDeactivate_(False)
        self.panel.setIgnoresMouseEvents_(True)
        self.panel.setCollectionBehavior_(
            NSWindowCollectionBehaviorCanJoinAllSpaces
            | NSWindowCollectionBehaviorFullScreenAuxiliary
        )
        self.panel.setOpaque_(False)
        self.panel.setBackgroundColor_(NSColor.clearColor())
        self.panel.setHasShadow_(True)
        view = self.panel.contentView()
        view.setWantsLayer_(True)
        view.layer().setCornerRadius_(18)
        view.layer().setBackgroundColor_(
            NSColor.colorWithCalibratedRed_green_blue_alpha_(0.13, 0.23, 0.37, 0.97).CGColor()
        )
        self.label = NSTextField.labelWithString_("Listening…")
        self.label.setFrame_(((16, 13), (258, 23)))
        self.label.setAlignment_(1)
        self.label.setFont_(NSFont.systemFontOfSize_weight_(14, 0.23))
        self.label.setTextColor_(NSColor.whiteColor())
        view.addSubview_(self.label)

    def _menu_item(self, menu, title, action, key=""):
        item = NSMenuItem.alloc().initWithTitle_action_keyEquivalent_(title, action, key)
        item.setTarget_(self.actions)
        menu.addItem_(item)
        return item

    def update(self, state, message):
        names = {
            "ready": "Ready",
            "setup": "Finish setup",
            "loading": "Setting up",
            "starting": "Opening microphone",
            "recording": "Listening",
            "tail": "Finishing",
            "stopping": "Finishing",
            "transcribing": "Transcribing",
        }
        self.status.setTitle_("Whisp · " + names[state])
        self.item.button().setToolTip_(message)
        recording = state in ("starting", "recording")
        self.record_item.setTitle_("Stop recording" if recording else "Start recording")
        self.record_item.setEnabled_(recording or state == "ready")
        self.cancel_item.setEnabled_(
            state in ("starting", "recording", "tail", "stopping", "transcribing")
        )
        if state in ("starting", "recording", "tail", "stopping", "transcribing"):
            self.label.setStringValue_(
                names[state] + (" · Esc to cancel" if state == "recording" else "…")
            )
            screen = NSScreen.mainScreen()
            if screen:
                frame = screen.visibleFrame()
                self.panel.setFrameOrigin_(
                    (frame.origin.x + (frame.size.width - 290) / 2, frame.origin.y + 28)
                )
            self.panel.orderFrontRegardless()
        else:
            self.panel.orderOut_(None)

    def close(self):
        self.panel.orderOut_(None)
        NSStatusBar.systemStatusBar().removeStatusItem_(self.item)
