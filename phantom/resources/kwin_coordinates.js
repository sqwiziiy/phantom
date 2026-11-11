var trackedWindow = null;
var trackedOutput = null;

function watchWindow(w) {
    if (!w) {
        return;
    }
    if (trackedWindow !== w) {
        trackedWindow = w;
        w.clientGeometryChanged.connect(publish);
        w.fullScreenChanged.connect(publish);
        w.outputChanged.connect(publish);
    }
    if (trackedOutput !== w.output && w.output) {
        trackedOutput = w.output;
        w.output.geometryChanged.connect(publish);
        w.output.scaleChanged.connect(publish);
    }
}

function publish() {
    var w = workspace.activeWindow;
    if (!w) {
        callDBus("org.phantom.Input", "/org/phantom/Input", "org.phantom.Input", "publish",
                 "", "", 0, 0, 0, 0, workspace.cursorPos.x, workspace.cursorPos.y, 1);
        return;
    }
    watchWindow(w);
    var g = w.clientGeometry;
    var scale = 1;
    if (w.output && (w.output.devicePixelRatio || w.output.scale)) {
        scale = w.output.devicePixelRatio || w.output.scale;
    }
    callDBus("org.phantom.Input", "/org/phantom/Input", "org.phantom.Input", "publish",
             w.resourceClass, w.caption, g.x, g.y, g.width, g.height,
             workspace.cursorPos.x, workspace.cursorPos.y, scale);
}

workspace.cursorPosChanged.connect(publish);
workspace.windowActivated.connect(publish);
if (workspace.windowAdded) {
    workspace.windowAdded.connect(publish);
    workspace.windowRemoved.connect(publish);
} else {
    workspace.clientAdded.connect(publish);
    workspace.clientRemoved.connect(publish);
}
workspace.screensChanged.connect(publish);
workspace.virtualScreenGeometryChanged.connect(publish);
publish();
