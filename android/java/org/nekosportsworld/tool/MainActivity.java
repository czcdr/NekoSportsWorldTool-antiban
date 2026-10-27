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
    /** verifyUpdatePackage 的哨兵返回值：签名与已安装版本不一致（需要卸载重装）。 */
    private static final String UPDATE_SIGNER_MISMATCH = "SIGNER_MISMATCH";
    /** 与 Rust 侧 src/update.rs 的 REPO_OWN 保持一致（自更新下载源）。 */
    private static final String RELEASES_URL =
        "https://github.com/czcdr/NekoSportsWorldTool-antiban/releases/latest";
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

    /** 自更新：校验后把私有目录里已下载的 update.apk 交给系统安装器。 */
    public void installApk(String path) {
        runOnUiThread(() -> {
            try {
                java.io.File apk = new java.io.File(path == null ? "" : path);
                if (path == null || !apk.isFile()) {
                    android.widget.Toast.makeText(this,
                        "安装包不存在，请重新检查更新",
                        android.widget.Toast.LENGTH_LONG).show();
                    return;
                }
                // 更新包在交给系统安装器之前先自己核一遍：包名、签名、版本号。
                // 系统只会拒绝"装不上"，不会告诉用户"这个包不是本应用的"；
                // 而摘要缺失/接口异常时 Rust 侧的 fail-open 分支曾经放行过未校验的包。
                String problem = verifyUpdatePackage(apk);
                if (problem != null) {
                    // 签名不一致要说清"为什么装不上、接下来怎么做"。旧逻辑只提示
                    // "校验失败，请重新下载"，换钥之后会变成下载→失败→再下载的死循环。
                    if (UPDATE_SIGNER_MISMATCH.equals(problem)) {
                        //noinspection ResultOfMethodCallIgnored
                        apk.delete();
                        showSignatureChangedDialog();
                        return;
                    }
                    android.widget.Toast.makeText(this,
                        "更新包校验失败：" + problem + "（已删除，请重新下载）",
                        android.widget.Toast.LENGTH_LONG).show();
                    //noinspection ResultOfMethodCallIgnored
                    apk.delete();
                    return;
                }
                // 用户实测「下载并安装后版本没变」：versionCode 相同/更低时系统安装器
                // 只弹「未安装」，看起来像沉默失败。先自己拦下来并说明原因。
                long incomingCode = incomingVersionCode(apk);
                long installedCode = currentVersionCode();
                if (incomingCode <= installedCode) {
                    showInstallBlockedDialog(
                        "这个更新包与已安装版本相同或更旧（versionCode "
                            + incomingCode + " ≤ " + installedCode + "）。\n\n"
                            + "系统不会用相同或更旧的版本覆盖安装，所以点多少次都不会变。\n"
                            + "请确认 Release 页的 tag 与包内版本号一致，或到 Release 页手动下载对应版本。",
                        apk);
                    return;
                }
                // 未授予「安装未知应用」时 startActivity 会"成功"但系统直接吞掉。
                if (android.os.Build.VERSION.SDK_INT >= 26
                        && !getPackageManager().canRequestPackageInstalls()) {
                    showInstallBlockedDialog(
                        "系统还没有允许本应用安装应用。\n\n"
                            + "点「去授权」打开设置，允许本应用「安装未知应用」后回来再点一次安装；\n"
                            + "也可以到文件管理器手动安装：" + apk.getAbsolutePath(),
                        apk);
                    return;
                }
                android.content.Intent intent = new android.content.Intent(android.content.Intent.ACTION_VIEW);
                intent.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
                intent.addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION);
                intent.setDataAndType(ApkProvider.uriForUpdate(), "application/vnd.android.package-archive");
                if (intent.resolveActivity(getPackageManager()) == null) {
                    showInstallBlockedDialog(
                        "这台设备上没有可用的应用安装器。\n\n"
                            + "请到系统设置允许本应用「安装未知应用」，或用文件管理器手动安装："
                            + apk.getAbsolutePath(),
                        apk);
                    return;
                }
                startActivity(intent);
            } catch (Exception error) {
                showInstallBlockedDialog(
                    "无法启动安装器：" + error.getClass().getSimpleName()
                        + (error.getMessage() == null ? "" : "（" + error.getMessage() + "）")
                        + "\n\n请到「设置 → 应用 → 本应用 → 安装未知应用」允许后重试，"
                        + "或用文件管理器手动安装应用私有目录下的 update.apk。",
                    new java.io.File(path == null ? "" : path));
            }
        });
    }

    /** 更新包的 versionCode（解析失败返回 -1）。 */
    private long incomingVersionCode(java.io.File apk) {
        try {
            android.content.pm.PackageInfo info = getPackageManager()
                .getPackageArchiveInfo(apk.getAbsolutePath(), 0);
            if (info == null) return -1;
            return android.os.Build.VERSION.SDK_INT >= 28 ? info.getLongVersionCode() : info.versionCode;
        } catch (Exception error) {
            return -1;
        }
    }

    /** 已安装版本的 versionCode。 */
    private long currentVersionCode() {
        try {
            android.content.pm.PackageInfo info =
                getPackageManager().getPackageInfo(getPackageName(), 0);
            return android.os.Build.VERSION.SDK_INT >= 28 ? info.getLongVersionCode() : info.versionCode;
        } catch (Exception error) {
            return -1;
        }
    }

    /**
     * 安装被拦下时统一出口：说清原因 + 给出两条能自己动手的路径
     * （去授权 / 文件管理器手动安装这个包的绝对路径）。
     */
    private void showInstallBlockedDialog(String message, java.io.File apk) {
        if (isFinishing() || isDestroyed()) return;
        final String path = apk == null ? "" : apk.getAbsolutePath();
        new AlertDialog.Builder(this)
            .setTitle("这次没能启动安装")
            .setMessage(message + (path.isEmpty() ? "" : "\n\n安装包路径：\n" + path))
            .setPositiveButton("知道了", null)
            .setNeutralButton("去授权", (dialog, which) -> {
                try {
                    startActivity(new android.content.Intent(
                        android.provider.Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                        android.net.Uri.parse("package:" + getPackageName())));
                } catch (Exception ignored) {
                    // 没有该设置页的 ROM：用户可自行去「设置 → 应用 → 安装未知应用」。
                }
            })
            .show();
    }

    /** 校验待安装包：包名一致、签名一致、versionCode 不低于当前。返回 null 表示通过。 */
    private String verifyUpdatePackage(java.io.File apk) {        android.content.pm.PackageManager pm = getPackageManager();
        int flags = android.os.Build.VERSION.SDK_INT >= 28
            ? android.content.pm.PackageManager.GET_SIGNING_CERTIFICATES
            : android.content.pm.PackageManager.GET_SIGNATURES;
        android.content.pm.PackageInfo archive = pm.getPackageArchiveInfo(apk.getAbsolutePath(), flags);
        if (archive == null) return "无法解析安装包（文件可能已损坏）";
        if (!getPackageName().equals(archive.packageName)) {
            return "包名不符（" + archive.packageName + "）";
        }
        android.content.pm.PackageInfo installed;
        try {
            installed = pm.getPackageInfo(getPackageName(), flags);
        } catch (Exception error) {
            return "读取当前版本信息失败：" + error.getMessage();
        }
        long incoming = android.os.Build.VERSION.SDK_INT >= 28
            ? archive.getLongVersionCode()
            : archive.versionCode;
        long current = android.os.Build.VERSION.SDK_INT >= 28
            ? installed.getLongVersionCode()
            : installed.versionCode;
        if (incoming < current) {
            return "版本号低于已安装版本（" + incoming + " < " + current + "）";
        }
        if (!sameSigner(archive, installed)) {
            return UPDATE_SIGNER_MISMATCH;
        }
        return null;
    }

    /**
     * 签名不一致：不是"文件坏了"，而是这个更新包与已安装版本不是同一把钥匙签的
     * （换钥发布，或有人塞了伪造包）。系统必然拒绝覆盖安装，必须换一条路径。
     */
    private void showSignatureChangedDialog() {
        if (isFinishing() || isDestroyed()) return;
        new AlertDialog.Builder(this)
            .setTitle("需要先卸载旧版")
            .setMessage(
                "这次更新换了 Android 签名密钥，系统不允许直接覆盖安装。\n\n"
                    + "请按顺序操作：\n"
                    + "1. 点「打开下载页」，把新版 APK 下载到手机的“下载”目录；\n"
                    + "2. 在系统设置里卸载本应用；\n"
                    + "3. 从“下载”目录安装刚下载的 APK。\n\n"
                    + "卸载会删除本地保存的账号与会话，需要重新登录。")
            .setPositiveButton("知道了", null)
            .setNegativeButton("打开下载页", (dialog, which) -> {
                try {
                    startActivity(new android.content.Intent(
                        android.content.Intent.ACTION_VIEW,
                        android.net.Uri.parse(RELEASES_URL)));
                } catch (Exception ignored) {
                    // 没有浏览器时保持对话框关闭即可，用户可自行访问 Release 页。
                }
            })
            .show();
    }

    private static boolean sameSigner(
        android.content.pm.PackageInfo a, android.content.pm.PackageInfo b) {
        byte[] left = firstSignerDigest(a);
        byte[] right = firstSignerDigest(b);
        return left != null && right != null && java.util.Arrays.equals(left, right);
    }

    private static byte[] firstSignerDigest(android.content.pm.PackageInfo info) {
        android.content.pm.Signature[] signatures = null;
        if (android.os.Build.VERSION.SDK_INT >= 28 && info.signingInfo != null) {
            signatures = info.signingInfo.hasMultipleSigners()
                ? info.signingInfo.getApkContentsSigners()
                : info.signingInfo.getSigningCertificateHistory();
        }
        if ((signatures == null || signatures.length == 0) && info.signatures != null) {
            signatures = info.signatures;
        }
        if (signatures == null || signatures.length == 0) return null;
        try {
            return java.security.MessageDigest.getInstance("SHA-256")
                .digest(signatures[0].toByteArray());
        } catch (Exception error) {
            return null;
        }
    }

    /** 路网文件选择：用系统文件选择器（SAF），把内容复制进私有目录后交给 Rust 解析。 */
    private static final int REQUEST_PICK_OSM = 0x0A51;

    /**
     * 本次选择请求的序号（由 Rust 侧生成并回传）。
     *
     * 结果回调是一条单槽通道：不带走序号时，大文件复制期间再次选择、
     * Activity 重建或旧线程晚到都会覆盖新结果 —— 表现为「加载了上一次的文件」
     * 或「取消把成功结果冲掉」。带上序号，Rust 侧只接受当前这次请求的回调。
     */
    private volatile int pendingPickId = 0;

    public void pickOsmFile(int requestId) {
        pendingPickId = requestId;
        runOnUiThread(() -> {
            try {
                android.content.Intent intent =
                    new android.content.Intent(android.content.Intent.ACTION_OPEN_DOCUMENT);
                intent.addCategory(android.content.Intent.CATEGORY_OPENABLE);
                intent.setType("*/*");
                startActivityForResult(intent, REQUEST_PICK_OSM);
            } catch (Exception error) {
                nativePickedFile(requestId, "", "无法打开文件选择器：" + error.getMessage());
            }
        });
    }

    @Override protected void onActivityResult(int requestCode, int resultCode, android.content.Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != REQUEST_PICK_OSM) return;
        final int requestId = pendingPickId;
        if (resultCode != RESULT_OK || data == null || data.getData() == null) {
            nativePickedFile(requestId, "", "已取消选择");
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
                // 先按声明大小拦一道：Rust 侧也是同一个上限（24 MiB），但那时文件已经
                // 写完，手机上会先把私有存储占满（另一个后果是写入过程中 UI 卡顿）。
                if (declaredSize > MAX_OSM_BYTES) {
                    nativePickedFile(requestId, "", "文件过大（"
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
                            nativePickedFile(requestId, "", "文件过大（超过 "
                                + (MAX_OSM_BYTES / (1024 * 1024)) + " MiB）；请先裁剪路网范围");
                            return;
                        }
                        out.write(buffer, 0, read);
                    }
                }
                nativePickedFile(requestId, target.getAbsolutePath(), "");
            } catch (Exception error) {
                nativePickedFile(requestId, "", "读取所选文件失败：" + error.getMessage());
            }
        }).start();
    }

    /**
     * 路网文件上限。
     *
     * 必须与 Rust 侧 `load_network_path` / `school_roads::MAX_BYTES` 的 24 MiB 一致：
     * 这里给 64 MiB 时，用户选 24–64 MiB 的文件会先被完整复制进私有存储，
     * 再被 Rust 拒绝 —— 白占存储，提示还互相矛盾。
     */
    private static final long MAX_OSM_BYTES = 24L * 1024 * 1024;

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

    /**
     * 文件选择结果回调：path 与 error 二选一。
     *
     * `requestId` 必须原样带回 —— Rust 侧只接受「当前这次请求」的回调，
     * 晚到的旧结果会被丢弃（否则大文件复制期间再次选择/旧线程晚到会覆盖新结果）。
     * 签名必须与 `src/android.rs::Java_..._nativePickedFile` 的 (jint, JString, JString) 一致。
     */
    public native void nativePickedFile(int requestId, String path, String error);

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
