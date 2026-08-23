import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * The window is undecorated so the chrome can match the app, which means the buttons and the
 * drag region are ours to provide.
 */
export function TitleBar() {
  const w = getCurrentWindow();
  return (
    <div className="titlebar">
      <div className="brand">
        <span className="dot" />
        SwiftLoad
      </div>
      <div className="drag" data-tauri-drag-region />
      <button className="winbtn" title="Minimise" onClick={() => void w.minimize()}>
        &#x2500;
      </button>
      <button className="winbtn" title="Maximise" onClick={() => void w.toggleMaximize()}>
        &#x25A1;
      </button>
      <button className="winbtn close" title="Close" onClick={() => void w.close()}>
        &#x2715;
      </button>
    </div>
  );
}
