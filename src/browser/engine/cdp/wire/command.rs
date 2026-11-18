//! Closed CDP command vocabulary for the existing v1 adapter.
//! This is protocol metadata, not the future cross-browser page interface.
use crate::browser::control::Effect;

#[derive(Clone, Copy)]
pub enum Command {
    AccessibilityEnable,
    AccessibilityGetPartialAXTree,
    BrowserGetVersion,
    BrowserSetDownloadBehavior,
    #[cfg(feature = "experimental-browser-bidi")]
    CssDisable,
    #[cfg(feature = "experimental-browser-bidi")]
    CssEnable,
    #[cfg(feature = "experimental-browser-bidi")]
    CssGetMatchedStylesForNode,
    #[cfg(feature = "experimental-browser-bidi")]
    DomDisable,
    #[cfg(feature = "experimental-browser-bidi")]
    DomEnable,
    DomDescribeNode,
    #[cfg(feature = "experimental-browser-bidi")]
    DomGetDocument,
    #[cfg(feature = "experimental-browser-bidi")]
    DomRequestNode,
    DomFocus,
    DomGetContentQuads,
    DomGetFrameOwner,
    DomGetNodeForLocation,
    DomResolveNode,
    DomScrollIntoViewIfNeeded,
    DomSetFileInputFiles,
    FetchContinueRequest,
    FetchEnable,
    FetchFailRequest,
    FetchTakeResponseBodyAsStream,
    InputKeyPress,
    InputKeyRelease,
    #[cfg(feature = "experimental-browser-bidi")]
    InputMouseMove,
    InputMousePress,
    InputMouseRelease,
    InputMouseWheel,
    InputInsertText,
    IoClose,
    IoRead,
    NetworkEnable,
    NetworkSetBlockedURLs,
    NetworkSetCacheDisabled,
    PageBringToFront,
    PageCaptureScreenshot,
    PageCreateIsolatedWorld,
    PageEnable,
    PageGetFrameTree,
    PageGetLayoutMetrics,
    PageHandleJavaScriptDialog,
    PageNavigate,
    PageSetInterceptFileChooserDialog,
    PageStopLoading,
    RuntimeCallFunction(Effect),
    RuntimeEvaluate(Effect),
    RuntimeReleaseObject,
    RuntimeReleaseObjectGroup,
    RuntimeRunIfWaitingForDebugger,
    TargetCloseTarget,
    TargetCreateTarget,
    TargetSetAutoAttach,
    TargetSetDiscoverTargets,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Dialog {
    Continue,
    Block,
    Release,
}

pub(super) struct Spec {
    pub method: &'static str,
    pub effect: Effect,
    pub dialog: Dialog,
}

impl Command {
    pub(super) fn spec(self) -> Spec {
        use Dialog::{Block, Continue, Release};
        use Effect::{None, Page};
        // Preserve v1 classifications explicitly. In particular, setup and
        // policy traffic do not join the input handoff barrier. Any change to
        // these semantics needs its own regression evidence, not a new prefix.
        let (method, effect, dialog) = match self {
            Self::AccessibilityEnable => ("Accessibility.enable", None, Continue),
            Self::AccessibilityGetPartialAXTree => {
                ("Accessibility.getPartialAXTree", None, Continue)
            }
            #[cfg(feature = "experimental-browser-bidi")]
            Self::CssDisable => ("CSS.disable", None, Continue),
            #[cfg(feature = "experimental-browser-bidi")]
            Self::CssEnable => ("CSS.enable", None, Continue),
            #[cfg(feature = "experimental-browser-bidi")]
            Self::CssGetMatchedStylesForNode => ("CSS.getMatchedStylesForNode", None, Continue),
            #[cfg(feature = "experimental-browser-bidi")]
            Self::DomDisable => ("DOM.disable", None, Continue),
            #[cfg(feature = "experimental-browser-bidi")]
            Self::DomEnable => ("DOM.enable", None, Continue),
            #[cfg(feature = "experimental-browser-bidi")]
            Self::DomGetDocument => ("DOM.getDocument", None, Continue),
            #[cfg(feature = "experimental-browser-bidi")]
            Self::DomRequestNode => ("DOM.requestNode", None, Continue),
            Self::BrowserGetVersion => ("Browser.getVersion", None, Continue),
            Self::BrowserSetDownloadBehavior => ("Browser.setDownloadBehavior", None, Continue),
            Self::DomDescribeNode => ("DOM.describeNode", None, Block),
            Self::DomFocus => ("DOM.focus", Page, Block),
            Self::DomGetContentQuads => ("DOM.getContentQuads", None, Block),
            Self::DomGetFrameOwner => ("DOM.getFrameOwner", None, Block),
            Self::DomGetNodeForLocation => ("DOM.getNodeForLocation", None, Block),
            Self::DomResolveNode => ("DOM.resolveNode", None, Block),
            Self::DomScrollIntoViewIfNeeded => ("DOM.scrollIntoViewIfNeeded", Page, Block),
            Self::DomSetFileInputFiles => ("DOM.setFileInputFiles", Page, Block),
            Self::FetchContinueRequest => ("Fetch.continueRequest", None, Continue),
            Self::FetchEnable => ("Fetch.enable", None, Continue),
            Self::FetchFailRequest => ("Fetch.failRequest", None, Continue),
            Self::FetchTakeResponseBodyAsStream => {
                ("Fetch.takeResponseBodyAsStream", None, Continue)
            }
            Self::InputKeyPress => ("Input.dispatchKeyEvent", Page, Block),
            Self::InputKeyRelease => ("Input.dispatchKeyEvent", Page, Release),
            #[cfg(feature = "experimental-browser-bidi")]
            Self::InputMouseMove => ("Input.dispatchMouseEvent", Page, Block),
            Self::InputMousePress | Self::InputMouseWheel => {
                ("Input.dispatchMouseEvent", Page, Block)
            }
            Self::InputMouseRelease => ("Input.dispatchMouseEvent", Page, Release),
            Self::InputInsertText => ("Input.insertText", Page, Block),
            Self::IoClose => ("IO.close", None, Continue),
            Self::IoRead => ("IO.read", None, Continue),
            Self::NetworkEnable => ("Network.enable", None, Continue),
            Self::NetworkSetBlockedURLs => ("Network.setBlockedURLs", None, Continue),
            Self::NetworkSetCacheDisabled => ("Network.setCacheDisabled", None, Continue),
            Self::PageBringToFront => ("Page.bringToFront", Page, Block),
            Self::PageCaptureScreenshot => ("Page.captureScreenshot", None, Block),
            Self::PageCreateIsolatedWorld => ("Page.createIsolatedWorld", None, Block),
            Self::PageEnable => ("Page.enable", None, Block),
            Self::PageGetFrameTree => ("Page.getFrameTree", None, Block),
            Self::PageGetLayoutMetrics => ("Page.getLayoutMetrics", None, Block),
            Self::PageHandleJavaScriptDialog => ("Page.handleJavaScriptDialog", Page, Continue),
            Self::PageNavigate => ("Page.navigate", Page, Block),
            Self::PageSetInterceptFileChooserDialog => {
                ("Page.setInterceptFileChooserDialog", None, Block)
            }
            Self::PageStopLoading => ("Page.stopLoading", None, Block),
            Self::RuntimeCallFunction(effect) => ("Runtime.callFunctionOn", effect, Block),
            Self::RuntimeEvaluate(effect) => ("Runtime.evaluate", effect, Block),
            Self::RuntimeReleaseObject => ("Runtime.releaseObject", None, Continue),
            Self::RuntimeReleaseObjectGroup => ("Runtime.releaseObjectGroup", None, Continue),
            Self::RuntimeRunIfWaitingForDebugger => {
                ("Runtime.runIfWaitingForDebugger", None, Continue)
            }
            Self::TargetCloseTarget => ("Target.closeTarget", Page, Continue),
            Self::TargetCreateTarget => ("Target.createTarget", Page, Continue),
            Self::TargetSetAutoAttach => ("Target.setAutoAttach", None, Continue),
            Self::TargetSetDiscoverTargets => ("Target.setDiscoverTargets", None, Continue),
        };
        Spec {
            method,
            effect,
            dialog,
        }
    }
}

#[derive(Clone, Copy)]
pub enum InputPair {
    Key,
    Mouse,
}

impl InputPair {
    pub(super) fn commands(self) -> (Command, Command) {
        match self {
            Self::Key => (Command::InputKeyPress, Command::InputKeyRelease),
            Self::Mouse => (Command::InputMousePress, Command::InputMouseRelease),
        }
    }
}
