package org.nekosportsworld.tool;

import android.app.AlertDialog;
import android.app.NativeActivity;
import android.os.Bundle;
import android.text.InputType;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;
import android.widget.FrameLayout;

/** Android UI thread owns the real text editor; Rust owns application state. */
public final class MainActivity extends NativeActivity {
    static { System.loadLibrary("nekosportsworldtool"); }
    private AlertDialog editorDialog;
    private AlertDialog deviceInfoDialog;
    private boolean taskActive;
    private boolean resumed;

    private native void nativeSubmitEdit(long id, String value);
    private native void nativeSetInsets(int left, int top, int right, int bottom);
    private native void nativeDeviceInfo(boolean initial, String info, String error);

    public void requestDeviceInfo(boolean initial) {
        runOnUiThread(() -> {
            android.content.SharedPreferences preferences = getSharedPreferences("device_info", MODE_PRIVATE);
            if (isFinishing() || isDestroyed() || deviceInfoDialog != null) return;
            if (initial && preferences.getBoolean("asked", false)) return;
            String purpose = initial
                ? "允许后会保存到设备身份，用于后续登录和业务请求。"
                : "允许后只填写设备页，点击“保存”后用于后续登录和业务请求。";
            AlertDialog dialog = new AlertDialog.Builder(this)
                .setTitle("读取本机信息")
                .setMessage("是否允许读取本机品牌、型号、系统版本、屏幕分辨率和本应用的 ANDROID_ID？\n\n" + purpose
                    + "品牌仅用于设备页展示；以上数值只用于让本应用上报的设备信息与这台手机保持一致。"
                    + "\n\n设备 UUID 继续沿用本应用首次生成并保存的值，不会更换。拒绝后仍可手动填写，也可在设备页再次读取。")
                .setPositiveButton("允许读取", (ignored, which) -> {
                    // First-run completion is acknowledged only after Rust saves it.
                    // If the Activity/process exits earlier, the next launch asks again.
                    if (!initial) preferences.edit().putBoolean("asked", true).apply();
                    try {
                        // These fields are read only after the user accepts this dialog.
                        org.json.JSONObject info = new org.json.JSONObject();
                        info.put("manufacturer", android.os.Build.MANUFACTURER);
                        info.put("model", android.os.Build.MODEL);
                        info.put("os_version", android.os.Build.VERSION.RELEASE);
                        // 分辨率与 ANDROID_ID：真机读到的值由构造即自洽，
                        // 比任何"机型对照表"都可靠（官方客户端在同一台机上上报的就是它们）。
                        android.util.DisplayMetrics metrics = getResources().getDisplayMetrics();
                        int w = Math.min(metrics.widthPixels, metrics.heightPixels);
                        int h = Math.max(metrics.widthPixels, metrics.heightPixels);
                        info.put("pixel", w > 0 && h > 0 ? (w + "x" + h) : "");
                        info.put("density_dpi", metrics.densityDpi);
                        String androidId = android.provider.Settings.Secure.getString(
                            getContentResolver(), android.provider.Settings.Secure.ANDROID_ID);
                        info.put("android_id", androidId == null ? "" : androidId);
                        nativeDeviceInfo(initial, info.toString(), "");
                    } catch (Exception error) {
                        nativeDeviceInfo(initial, "", "读取本机信息失败，请在设备页重试或手动填写");
                    }
                })
                .setNegativeButton("暂不读取", (ignored, which) -> {
                    preferences.edit().putBoolean("asked", true).apply();
                    nativeDeviceInfo(initial, "", "");
                })
                .create();
            dialog.setOnCancelListener(ignored -> {
                preferences.edit().putBoolean("asked", true).apply();
                nativeDeviceInfo(initial, "", "");
            });
            dialog.setOnDismissListener(ignored -> deviceInfoDialog = null);
            deviceInfoDialog = dialog;
            dialog.show();
        });
    }

    public void completeDeviceInfo() {
        getSharedPreferences("device_info", MODE_PRIVATE).edit().putBoolean("asked", true).apply();
    }

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        // NativeActivity's rendering surface must stay inside system bars.
        View content = findViewById(android.R.id.content);
        content.setOnApplyWindowInsetsListener((view, insets) -> {
            if (android.os.Build.VERSION.SDK_INT >= 30) {
                android.graphics.Insets bars = insets.getInsets(
                    WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
                nativeSetInsets(bars.left, bars.top, bars.right, bars.bottom);
            } else {
                nativeSetInsets(insets.getSystemWindowInsetLeft(), insets.getSystemWindowInsetTop(),
                    insets.getSystemWindowInsetRight(), insets.getSystemWindowInsetBottom());
            }
            return insets;
        });
        content.requestApplyInsets();
        applySystemBarStyle();
    }

    @Override public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) applySystemBarStyle();
    }

    private void applySystemBarStyle() {
        if (android.os.Build.VERSION.SDK_INT >= 30) {
            getWindow().getInsetsController().setSystemBarsAppearance(
                android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS |
                android.view.WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS,
                android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS |
                android.view.WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS);
        } else {
            getWindow().getDecorView().setSystemUiVisibility(
                View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR | View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR);
        }
    }

    public void openEditor(long id, String initialValue, int kind) {
        runOnUiThread(() -> {
            if (isFinishing() || isDestroyed() || editorDialog != null) return;
            EditText input = new EditText(this);
            input.setId(android.R.id.edit);
            int inputType;
            switch (kind) {
                case 1: inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_PASSWORD; break;
                case 2: inputType = InputType.TYPE_CLASS_NUMBER | InputType.TYPE_NUMBER_FLAG_SIGNED; break;
                case 3: inputType = InputType.TYPE_CLASS_NUMBER | InputType.TYPE_NUMBER_FLAG_SIGNED | InputType.TYPE_NUMBER_FLAG_DECIMAL; break;
                default: inputType = InputType.TYPE_CLASS_TEXT;
            }
            input.setInputType(inputType);
            input.setSingleLine(true);
            input.setImeOptions(EditorInfo.IME_ACTION_DONE | EditorInfo.IME_FLAG_NO_EXTRACT_UI);
            input.setText(initialValue);
            input.selectAll();
            int margin = Math.round(20 * getResources().getDisplayMetrics().density);
            FrameLayout container = new FrameLayout(this);
            container.setPadding(margin, 0, margin, 0);
            container.addView(input, new FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.WRAP_CONTENT));
            AlertDialog dialog = new AlertDialog.Builder(this)
                .setTitle(kind == 1 ? "输入密码" : "编辑内容")
                .setView(container)
                .setPositiveButton("确定", (ignored, which) -> nativeSubmitEdit(id, input.getText().toString()))
                .setNegativeButton("取消", null)
                .create();
            editorDialog = dialog;
            dialog.setOnDismissListener(ignored -> {
                InputMethodManager ime = (InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
                ime.hideSoftInputFromWindow(input.getWindowToken(), 0);
                editorDialog = null;
            });
            input.setOnEditorActionListener((view, action, event) -> {
                if (action != EditorInfo.IME_ACTION_DONE) return false;
                nativeSubmitEdit(id, input.getText().toString());
                dialog.dismiss();
                return true;
            });
            dialog.setOnShowListener(ignored -> {
                input.requestFocus();
                dialog.getWindow().setSoftInputMode(
                    WindowManager.LayoutParams.SOFT_INPUT_STATE_ALWAYS_VISIBLE |
                    WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE);
                input.post(() -> ((InputMethodManager) getSystemService(INPUT_METHOD_SERVICE))
                    .showSoftInput(input, InputMethodManager.SHOW_IMPLICIT));
            });
            dialog.show();
        });
    }

    public void setTaskActive(boolean active) {
        runOnUiThread(() -> {
            taskActive = active;
            updateScreenPolicy();
        });
    }

    public void copyText(String text) {
        runOnUiThread(() -> {
            android.content.ClipboardManager clipboard =
                (android.content.ClipboardManager) getSystemService(CLIPBOARD_SERVICE);
            clipboard.setPrimaryClip(android.content.ClipData.newPlainText("NekoSportsWorldTool", text));
        });
    }

    /** 自更新：把私有目录里已下载的 update.apk 交给系统安装器。 */
    public void installApk(String path) {
        runOnUiThread(() -> {
            try {
                if (path == null || !new java.io.File(path).isFile()) {
                    android.widget.Toast.makeText(this,
                        "安装包不存在，请重新检查更新",
                        android.widget.Toast.LENGTH_LONG).show();
                    return;
                }
                android.content.Intent intent = new android.content.Intent(android.content.Intent.ACTION_VIEW);
                intent.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
                intent.addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION);
                intent.setDataAndType(ApkProvider.uriForUpdate(), "application/vnd.android.package-archive");
                startActivity(intent);
            } catch (Exception error) {
                android.widget.Toast.makeText(this,
                    "无法启动安装器，请到系统设置允许本应用安装未知应用后重试",
                    android.widget.Toast.LENGTH_LONG).show();
            }
        });
    }

    /** 路网文件选择：用系统文件选择器（SAF），把内容复制进私有目录后交给 Rust 解析。 */
    private static final int REQUEST_PICK_OSM = 0x0A51;

    public void pickOsmFile() {
        runOnUiThread(() -> {
            try {
                android.content.Intent intent =
                    new android.content.Intent(android.content.Intent.ACTION_OPEN_DOCUMENT);
                intent.addCategory(android.content.Intent.CATEGORY_OPENABLE);
                intent.setType("*/*");
                startActivityForResult(intent, REQUEST_PICK_OSM);
            } catch (Exception error) {
                nativePickedFile("", "无法打开文件选择器：" + error.getMessage());
            }
        });
    }

    @Override protected void onActivityResult(int requestCode, int resultCode, android.content.Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != REQUEST_PICK_OSM) return;
        if (resultCode != RESULT_OK || data == null || data.getData() == null) {
            nativePickedFile("", "已取消选择");
            return;
        }
        final android.net.Uri uri = data.getData();
        // 复制动作放到后台线程：路网文件几十 MB，主线程读会把界面卡死。
        new Thread(() -> {
            try {
                String name = "imported.osm";
                long declaredSize = -1L;
                try (android.database.Cursor cursor =
                         getContentResolver().query(uri, null, null, null, null)) {
                    if (cursor != null && cursor.moveToFirst()) {
                        int index = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME);
                        if (index >= 0) {
                            String shown = cursor.getString(index);
                            if (shown != null && !shown.isEmpty()) name = shown;
                        }
                        int sizeIndex = cursor.getColumnIndex(android.provider.OpenableColumns.SIZE);
                        if (sizeIndex >= 0 && !cursor.isNull(sizeIndex)) {
                            declaredSize = cursor.getLong(sizeIndex);
                        }
                    }
                }
                // 先按声明大小拦一道：Rust 侧也有 64 MiB 上限，但那时文件已经写完，
                // 手机上会先把私有存储占满（另一个后果是写入过程中 UI 卡顿）。
                if (declaredSize > MAX_OSM_BYTES) {
                    nativePickedFile("", "文件过大（"
                        + (declaredSize / (1024 * 1024)) + " MiB，上限 "
                        + (MAX_OSM_BYTES / (1024 * 1024)) + " MiB）；请先裁剪路网范围");
                    return;
                }
                java.io.File dir = new java.io.File(getFilesDir(), "osm");
                //noinspection ResultOfMethodCallIgnored
                dir.mkdirs();
                // 文件名加时间戳：同名重选时不能截断上一个还在被解析的文件。
                String stamp = Long.toString(System.currentTimeMillis());
                java.io.File target = new java.io.File(dir, stamp + "-" + sanitizeFileName(name));
                long copied = 0L;
                try (java.io.InputStream in = getContentResolver().openInputStream(uri);
                     java.io.FileOutputStream out = new java.io.FileOutputStream(target)) {
                    if (in == null) throw new java.io.IOException("无法读取所选文件");
                    byte[] buffer = new byte[64 * 1024];
                    int read;
                    while ((read = in.read(buffer)) > 0) {
                        copied += read;
                        // 声明大小可能缺失或撒谎，边写边兜底。
                        if (copied > MAX_OSM_BYTES) {
                            out.close();
                            //noinspection ResultOfMethodCallIgnored
                            target.delete();
                            nativePickedFile("", "文件过大（超过 "
                                + (MAX_OSM_BYTES / (1024 * 1024)) + " MiB）；请先裁剪路网范围");
                            return;
                        }
                        out.write(buffer, 0, read);
                    }
                }
                nativePickedFile(target.getAbsolutePath(), "");
            } catch (Exception error) {
                nativePickedFile("", "读取所选文件失败：" + error.getMessage());
            }
        }).start();
    }

    /** 路网文件上限，与 Rust 侧 `load_network_path` 的 64 MiB 保持一致。 */
    private static final long MAX_OSM_BYTES = 64L * 1024 * 1024;

    private static String sanitizeFileName(String name) {
        java.io.File base = new java.io.File(name);
        String leaf = base.getName();
        StringBuilder builder = new StringBuilder();
        for (char ch : leaf.toCharArray()) {
            if (Character.isLetterOrDigit(ch) || ch == '.' || ch == '_' || ch == '-') builder.append(ch);
        }
        String out = builder.toString();
        // "." 与 ".." 会指向目录自身/父目录，new File(dir, "..") 必然写失败。
        if (out.isEmpty() || out.equals(".") || out.equals("..")) return "imported.osm";
        return out;
    }

    /** 文件选择结果回调：path 与 error 二选一。 */
    public native void nativePickedFile(String path, String error);

    /** 当前 APK 的 versionName（与 Release tag 对齐，供更新检查比较）。 */
    public String appVersionName() {
        try {
            return getPackageManager().getPackageInfo(getPackageName(), 0).versionName;
        } catch (Exception error) {
            return "";
        }
    }

    private void updateScreenPolicy() {
        if (taskActive && resumed) getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        else getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
    }

    @Override protected void onResume() {
        super.onResume();
        resumed = true;
        updateScreenPolicy();
    }

    @Override protected void onPause() {
        resumed = false;
        updateScreenPolicy();
        super.onPause();
    }

    @Override protected void onDestroy() {
        if (editorDialog != null) editorDialog.dismiss();
        if (deviceInfoDialog != null) deviceInfoDialog.dismiss();
        taskActive = false;
        updateScreenPolicy();
        super.onDestroy();
    }
}
