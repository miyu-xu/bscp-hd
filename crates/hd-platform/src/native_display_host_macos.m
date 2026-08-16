#import <AppKit/AppKit.h>
#import <QuartzCore/QuartzCore.h>
#import <UniformTypeIdentifiers/UniformTypeIdentifiers.h>
#import <dispatch/dispatch.h>
#import <objc/runtime.h>

#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

@interface CALayerHost : CALayer
@property(nonatomic) uint32_t contextId;
@end

enum {
    HD_CA_HELLO_MAGIC = 0x48444341,
    HD_CA_HELLO_V2_MAGIC = 0x48444342,
    HD_CA_SELECT_MAGIC = 0x4844534C,
    HD_INPUT_KEY = 1,
    HD_INPUT_TOUCH_DOWN = 2,
    HD_INPUT_TOUCH_MOVE = 3,
    HD_INPUT_TOUCH_UP = 4,
    HD_INPUT_VIEWPORT_RESIZE = 5,
    HD_INPUT_SELECT_DISPLAY = 6,
};

typedef struct {
    uint32_t magic;
    uint32_t context_id;
    uint32_t guest_width;
    uint32_t guest_height;
} HDCaHello;

typedef struct {
    uint32_t magic;
    uint32_t scanout_id;
    uint32_t context_id;
    uint32_t guest_width;
    uint32_t guest_height;
} HDCaHelloV2;

typedef struct {
    uint32_t magic;
    uint32_t scanout_id;
    uint32_t guest_width;
    uint32_t guest_height;
} HDCaSelect;

typedef struct {
    uint32_t magic;
    uint32_t status;
} HDCaSelectResponse;

typedef struct {
    int32_t kind;
    int32_t code;
    int32_t value;
    int32_t repeat;
    int32_t x;
    int32_t y;
} HDInputEvent;

typedef void (*HDTitlebarCallback)(void* context, const char* message);
static char HDTitlebarButtonMessageKey;
static char HDTitlebarFpsLabelKey;
static char HDTitlebarSidebarButtonKey;
static char HDTitlebarActionButtonsKey;
static char HDTitlebarSeparatorKey;
static char HDTitlebarLeftStackKey;
static char HDTitlebarRightStackKey;
static char HDTitlebarTargetKey;
static char HDTitlebarExpansionObserversKey;
static char HDWorkspaceLifecycleObserversKey;

extern void hd_macos_map_pointer_contract(double normalized_x,
                                           double normalized_y,
                                           uint8_t rotation_quarters,
                                           uint32_t guest_width,
                                           uint32_t guest_height,
                                           int32_t* output_x,
                                           int32_t* output_y);

typedef struct {
    const char* symbol;
    const char* fallback;
    const char* tooltip;
    const char* message;
    const char* placement;
} HDTitlebarControlContract;

static const HDTitlebarControlContract HDTitlebarControlContracts[] = {
    {"sidebar.left", "☰", "展开/折叠侧栏",
     "{\"command\":\"toggle_sidebar\"}", "left"},
    {"power", "⏻", "电源",
     "{\"command\":\"key\",\"key\":\"power\"}", "right"},
    {"speaker.wave.1", "−", "音量减",
     "{\"command\":\"key\",\"key\":\"volume_down\"}", "right"},
    {"speaker.wave.3", "+", "音量加",
     "{\"command\":\"key\",\"key\":\"volume_up\"}", "right"},
    {"rotate.right", "↻", "旋转",
     "{\"command\":\"rotate\"}", "right"},
    {"camera", "▣", "截图",
     "{\"command\":\"screenshot\"}", "right"},
    {"record.circle", "●", "开始录屏（最长 3 分钟）",
     "{\"command\":\"start_screen_recording\"}", "right"},
    {"shippingbox.and.arrow.backward", "↓", "安装 APK",
     "{\"command\":\"choose_install_apk\"}", "right"},
    {"rectangle.stack", "▢", "最近任务",
     "{\"command\":\"key\",\"key\":\"recent\"}", "right"},
    {"house", "⌂", "主页",
     "{\"command\":\"key\",\"key\":\"home\"}", "right"},
    {"chevron.left", "‹", "返回",
     "{\"command\":\"key\",\"key\":\"back\"}", "right"},
};

bool hd_macos_configure_application_startup(void) {
    if (![NSThread isMainThread]) {
        return false;
    }

    // HD reconstructs its one native window from the current Host/instance state after winit
    // enters Resumed. AppKit state restoration is therefore both redundant and unsafe: after an
    // interrupted development or product run, NSPersistentUIRestorer may present a modal
    // "discard restored windows" alert before winit installs its event handler. Put the standard
    // launch override in the volatile argument domain so it takes precedence for this process
    // without changing the user's persistent defaults.
    NSUserDefaults* defaults = NSUserDefaults.standardUserDefaults;
    NSMutableDictionary* argumentDomain =
        [[defaults volatileDomainForName:NSArgumentDomain] mutableCopy];
    if (argumentDomain == nil) {
        argumentDomain = [[NSMutableDictionary alloc] init];
    }
    argumentDomain[@"ApplePersistenceIgnoreState"] = @YES;
    [defaults setVolatileDomain:argumentDomain forName:NSArgumentDomain];
    return [defaults boolForKey:@"ApplePersistenceIgnoreState"];
}

size_t hd_macos_titlebar_control_count(void) {
    return sizeof(HDTitlebarControlContracts) / sizeof(HDTitlebarControlContracts[0]);
}

const char* hd_macos_titlebar_control_symbol(size_t index) {
    return index < hd_macos_titlebar_control_count()
        ? HDTitlebarControlContracts[index].symbol : NULL;
}

const char* hd_macos_titlebar_control_tooltip(size_t index) {
    return index < hd_macos_titlebar_control_count()
        ? HDTitlebarControlContracts[index].tooltip : NULL;
}

const char* hd_macos_titlebar_control_message(size_t index) {
    return index < hd_macos_titlebar_control_count()
        ? HDTitlebarControlContracts[index].message : NULL;
}

const char* hd_macos_titlebar_control_placement(size_t index) {
    return index < hd_macos_titlebar_control_count()
        ? HDTitlebarControlContracts[index].placement : NULL;
}

bool hd_macos_show_error_dialog(const char* title,
                                const char* message,
                                const char* reveal_path) {
    if (title == NULL || message == NULL) {
        return false;
    }
    NSString* alertTitle = [NSString stringWithUTF8String:title];
    NSString* alertMessage = [NSString stringWithUTF8String:message];
    NSString* revealPath =
        reveal_path == NULL ? nil : [NSString stringWithUTF8String:reveal_path];
    if (alertTitle == nil || alertMessage == nil ||
        (reveal_path != NULL && revealPath == nil)) {
        return false;
    }

    __block bool succeeded = false;
    void (^present)(void) = ^{
        @autoreleasepool {
            NSApplication* application = [NSApplication sharedApplication];
            [application setActivationPolicy:NSApplicationActivationPolicyRegular];
            [application activateIgnoringOtherApps:YES];

            NSAlert* alert = [[NSAlert alloc] init];
            alert.alertStyle = NSAlertStyleCritical;
            alert.messageText = alertTitle;
            alert.informativeText = alertMessage;
            [alert addButtonWithTitle:@"退出"];
            if (revealPath.length != 0) {
                [alert addButtonWithTitle:@"打开日志目录"];
            }
            NSModalResponse response = [alert runModal];
            if (response == NSAlertSecondButtonReturn && revealPath.length != 0) {
                NSURL* url = [NSURL fileURLWithPath:revealPath isDirectory:YES];
                [[NSWorkspace sharedWorkspace] activateFileViewerSelectingURLs:@[url]];
            }
            succeeded = true;
        }
    };
    if ([NSThread isMainThread]) {
        present();
    } else {
        dispatch_sync(dispatch_get_main_queue(), present);
    }
    return succeeded;
}

int32_t hd_macos_activate_existing_application(void) {
    if (![NSThread isMainThread]) {
        return -1;
    }
    NSString* bundleIdentifier = NSBundle.mainBundle.bundleIdentifier;
    if (bundleIdentifier.length == 0) {
        return -1;
    }
    pid_t currentPID = getpid();
    for (NSRunningApplication* application in
         [NSRunningApplication runningApplicationsWithBundleIdentifier:bundleIdentifier]) {
        if (application.processIdentifier == currentPID || application.terminated) {
            continue;
        }
        [application activateWithOptions:NSApplicationActivateAllWindows];
        return 1;
    }
    return 0;
}

static bool hd_read_full(int fd, void* buffer, size_t length) {
    uint8_t* cursor = buffer;
    while (length != 0) {
        ssize_t count = read(fd, cursor, length);
        if (count == 0) {
            return false;
        }
        if (count < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        cursor += count;
        length -= (size_t)count;
    }
    return true;
}

@interface HDNativeDisplayView : NSView
@property(atomic) int clientFD;
@property(atomic) uint32_t guestWidth;
@property(atomic) uint32_t guestHeight;
@property(atomic) uint8_t displayRotation;
@property(atomic) CGFloat renderX;
@property(atomic) CGFloat renderY;
@property(atomic) CGFloat renderWidth;
@property(atomic) CGFloat renderHeight;
@end

@implementation HDNativeDisplayView

- (BOOL)acceptsFirstResponder {
    return YES;
}

- (BOOL)isFlipped {
    return YES;
}

- (void)sendInput:(HDInputEvent)event {
    int fd = self.clientFD;
    if (fd < 0) {
        return;
    }
    const uint8_t* cursor = (const uint8_t*)&event;
    size_t remaining = sizeof(event);
    while (remaining != 0) {
        ssize_t count = send(fd, cursor, remaining, 0);
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count <= 0) {
            self.clientFD = -1;
            return;
        }
        cursor += count;
        remaining -= (size_t)count;
    }
}

- (int32_t)modifierPressed:(NSEvent*)event {
    NSEventModifierFlags flags = event.modifierFlags;
    switch (event.keyCode) {
        case 54:
        case 55:
            return (flags & NSEventModifierFlagCommand) != 0;
        case 56:
        case 60:
            return (flags & NSEventModifierFlagShift) != 0;
        case 57:
            return (flags & NSEventModifierFlagCapsLock) != 0;
        case 58:
        case 61:
            return (flags & NSEventModifierFlagOption) != 0;
        case 59:
        case 62:
            return (flags & NSEventModifierFlagControl) != 0;
        default:
            return 0;
    }
}

- (void)keyDown:(NSEvent*)event {
    HDInputEvent input = {
        .kind = HD_INPUT_KEY,
        .code = event.keyCode,
        .value = 1,
        .repeat = event.isARepeat,
    };
    [self sendInput:input];
}

- (void)keyUp:(NSEvent*)event {
    HDInputEvent input = {
        .kind = HD_INPUT_KEY,
        .code = event.keyCode,
        .value = 0,
    };
    [self sendInput:input];
}

- (void)flagsChanged:(NSEvent*)event {
    HDInputEvent input = {
        .kind = HD_INPUT_KEY,
        .code = event.keyCode,
        .value = [self modifierPressed:event],
    };
    [self sendInput:input];
}

- (void)sendPointer:(NSEvent*)event kind:(int32_t)kind {
    NSPoint point = [self convertPoint:event.locationInWindow fromView:nil];
    double renderX = self.renderX;
    double renderY = self.renderY;
    double width = MAX(self.renderWidth, 1.0);
    double height = MAX(self.renderHeight, 1.0);
    uint32_t guestWidth = MAX(self.guestWidth, 1);
    uint32_t guestHeight = MAX(self.guestHeight, 1);
    double x = point.x - renderX;
    double y = point.y - renderY;
    double normalizedX = MAX(0.0, MIN(1.0, x / width));
    // HDNativeDisplayView is flipped, so normalized Y grows down from the upper-left.
    double normalizedY = MAX(0.0, MIN(1.0, y / height));
    int32_t guestX = 0;
    int32_t guestY = 0;
    hd_macos_map_pointer_contract(normalizedX, normalizedY,
                                  self.displayRotation, guestWidth, guestHeight,
                                  &guestX, &guestY);
    HDInputEvent input = {
        .kind = kind,
        .x = guestX,
        .y = guestY,
    };
    [self sendInput:input];
}

- (void)mouseDown:(NSEvent*)event {
    [self.window makeFirstResponder:self];
    [self sendPointer:event kind:HD_INPUT_TOUCH_DOWN];
}

- (void)mouseDragged:(NSEvent*)event {
    [self sendPointer:event kind:HD_INPUT_TOUCH_MOVE];
}

- (void)mouseUp:(NSEvent*)event {
    [self sendPointer:event kind:HD_INPUT_TOUCH_UP];
}

@end

@interface HDTitlebarControls : NSObject
@property(nonatomic) void* context;
@property(nonatomic) HDTitlebarCallback callback;
- (instancetype)initWithContext:(void*)context callback:(HDTitlebarCallback)callback;
- (void)buttonPressed:(id)sender;
@end

@implementation HDTitlebarControls

- (instancetype)initWithContext:(void*)context callback:(HDTitlebarCallback)callback {
    self = [super init];
    if (self != nil) {
        self.context = context;
        self.callback = callback;
    }
    return self;
}

- (void)buttonPressed:(id)sender {
    if (self.callback == NULL) {
        return;
    }
    id represented = objc_getAssociatedObject(sender, &HDTitlebarButtonMessageKey);
    if (![represented isKindOfClass:[NSString class]]) {
        return;
    }
    self.callback(self.context, [(NSString*)represented UTF8String]);
}

@end

static NSButton* hd_titlebar_button(NSString* symbol,
                                    NSString* fallback,
                                    NSString* tooltip,
                                    NSString* message,
                                    HDTitlebarControls* target) {
    NSButton* button =
        [NSButton buttonWithTitle:fallback target:target action:@selector(buttonPressed:)];
    objc_setAssociatedObject(button, &HDTitlebarButtonMessageKey, message,
                             OBJC_ASSOCIATION_COPY_NONATOMIC);
    button.toolTip = tooltip;
    button.bordered = NO;
    button.imagePosition = NSImageOnly;
    button.bezelStyle = NSBezelStyleTexturedRounded;
    button.translatesAutoresizingMaskIntoConstraints = NO;
    if (@available(macOS 11.0, *)) {
        NSImage* image = [NSImage imageWithSystemSymbolName:symbol accessibilityDescription:tooltip];
        if (image != nil) {
            button.image = image;
        }
    }
    [button.widthAnchor constraintEqualToConstant:27.0].active = YES;
    [button.heightAnchor constraintEqualToConstant:27.0].active = YES;
    return button;
}

static NSButton* hd_titlebar_contract_button(size_t index,
                                             HDTitlebarControls* target) {
    const HDTitlebarControlContract* control = &HDTitlebarControlContracts[index];
    return hd_titlebar_button(
        [NSString stringWithUTF8String:control->symbol],
        [NSString stringWithUTF8String:control->fallback],
        [NSString stringWithUTF8String:control->tooltip],
        [NSString stringWithUTF8String:control->message],
        target);
}

static NSView* hd_titlebar_separator(void) {
    NSBox* separator = [[NSBox alloc] initWithFrame:NSMakeRect(0, 0, 1, 18)];
    separator.boxType = NSBoxSeparator;
    separator.translatesAutoresizingMaskIntoConstraints = NO;
    [separator.widthAnchor constraintEqualToConstant:1.0].active = YES;
    [separator.heightAnchor constraintEqualToConstant:18.0].active = YES;
    return separator;
}

static void hd_fit_titlebar_stack(NSStackView* stack) {
    if (stack == nil) {
        return;
    }
    [stack invalidateIntrinsicContentSize];
    [stack layoutSubtreeIfNeeded];
    NSSize fitting = stack.fittingSize;
    NSRect frame = stack.frame;
    frame.size.width = ceil(MAX(fitting.width, 1.0));
    frame.size.height = 32.0;
    stack.frame = frame;
}

@interface HDMacNativeDisplayHost : NSObject
@property(nonatomic, weak) NSView* parentView;
@property(nonatomic, strong) HDNativeDisplayView* displayView;
@property(nonatomic, strong) CALayer* containerLayer;
@property(nonatomic, strong) CALayerHost* layerHost;
@property(nonatomic, copy) NSString* endpoint;
@property(nonatomic) BOOL ownsEndpoint;
@property(atomic) int lockFD;
@property(atomic) int listenFD;
@property(nonatomic, strong) NSMutableDictionary<NSNumber*, id>* connections;
@property(atomic) uint32_t selectedScanout;
- (instancetype)initWithParent:(NSView*)parent endpoint:(NSString*)endpoint;
- (BOOL)setX:(int32_t)x
           y:(int32_t)y
       width:(uint32_t)width
      height:(uint32_t)height
    rotation:(uint8_t)rotation
     visible:(BOOL)visible;
- (void)layoutHostedLayer;
- (void)shutdown;
@end

@interface HDCaConnection : NSObject
@property(nonatomic) int fd;
@property(nonatomic) uint32_t contextId;
@property(nonatomic) uint32_t guestWidth;
@property(nonatomic) uint32_t guestHeight;
@end

@implementation HDCaConnection
@end

@implementation HDMacNativeDisplayHost

- (instancetype)initWithParent:(NSView*)parent endpoint:(NSString*)endpoint {
    self = [super init];
    if (self == nil) {
        return nil;
    }
    self.parentView = parent;
    self.endpoint = endpoint;
    self.ownsEndpoint = NO;
    self.lockFD = -1;
    self.listenFD = -1;
    self.connections = [[NSMutableDictionary alloc] init];
    self.selectedScanout = 0;
    parent.wantsLayer = YES;
    parent.layer.backgroundColor = NSColor.blackColor.CGColor;

    HDNativeDisplayView* view =
        [[HDNativeDisplayView alloc] initWithFrame:NSMakeRect(0, 0, 1, 1)];
    view.clientFD = -1;
    view.guestWidth = 1;
    view.guestHeight = 1;
    view.displayRotation = 0;
    view.renderX = 0;
    view.renderY = 0;
    view.renderWidth = 1;
    view.renderHeight = 1;
    view.hidden = YES;
    view.wantsLayer = YES;

    CALayer* containerLayer = [CALayer layer];
    containerLayer.backgroundColor = NSColor.blackColor.CGColor;
    containerLayer.masksToBounds = YES;
    view.layer = containerLayer;

    CALayerHost* layerHost = [CALayerHost layer];
    layerHost.backgroundColor = NSColor.blackColor.CGColor;
    layerHost.anchorPoint = CGPointMake(0.0, 0.0);
    layerHost.masksToBounds = YES;
    layerHost.magnificationFilter = kCAFilterLinear;
    layerHost.minificationFilter = kCAFilterLinear;
    [containerLayer addSublayer:layerHost];

    [parent addSubview:view positioned:NSWindowBelow relativeTo:nil];
    self.displayView = view;
    self.containerLayer = containerLayer;
    self.layerHost = layerHost;

    const char* path = endpoint.fileSystemRepresentation;
    if (path == NULL || strlen(path) >= sizeof(((struct sockaddr_un*)0)->sun_path)) {
        [view removeFromSuperview];
        return nil;
    }
    NSString* lockEndpoint = [endpoint stringByAppendingString:@".lock"];
    int lockFD = open(lockEndpoint.fileSystemRepresentation, O_CREAT | O_RDWR, S_IRUSR | S_IWUSR);
    if (lockFD < 0 || flock(lockFD, LOCK_EX | LOCK_NB) != 0) {
        if (lockFD >= 0) {
            close(lockFD);
        }
        [view removeFromSuperview];
        return nil;
    }
    self.lockFD = lockFD;
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        flock(lockFD, LOCK_UN);
        close(lockFD);
        self.lockFD = -1;
        [view removeFromSuperview];
        return nil;
    }
    int closeOnExec = fcntl(fd, F_GETFD);
    if (closeOnExec >= 0) {
        (void)fcntl(fd, F_SETFD, closeOnExec | FD_CLOEXEC);
    }
    (void)unlink(path);
    struct sockaddr_un address = {0};
    address.sun_family = AF_UNIX;
    strlcpy(address.sun_path, path, sizeof(address.sun_path));
    if (bind(fd, (const struct sockaddr*)&address, sizeof(address)) != 0 ||
        chmod(path, S_IRUSR | S_IWUSR) != 0 || listen(fd, 8) != 0) {
        close(fd);
        (void)unlink(path);
        flock(lockFD, LOCK_UN);
        close(lockFD);
        self.lockFD = -1;
        [view removeFromSuperview];
        return nil;
    }
    self.listenFD = fd;
    self.ownsEndpoint = YES;

    __weak HDMacNativeDisplayHost* weakSelf = self;
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
        while (true) {
            HDMacNativeDisplayHost* strongSelf = weakSelf;
            if (strongSelf == nil || strongSelf.listenFD != fd) {
                return;
            }
            int client = accept(fd, NULL, NULL);
            if (client < 0) {
                if (errno == EINTR) {
                    continue;
                }
                return;
            }
            int noSigPipe = 1;
            (void)setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, sizeof(noSigPipe));
            uint32_t magic = 0;
            if (!hd_read_full(client, &magic, sizeof(magic))) {
                close(client);
                continue;
            }
            if (magic == HD_CA_SELECT_MAGIC) {
                HDCaSelect select = {.magic = magic, .scanout_id = 0,
                                     .guest_width = 0, .guest_height = 0};
                if (!hd_read_full(client, &select.scanout_id,
                                  sizeof(select) - sizeof(select.magic))) {
                    close(client);
                    continue;
                }
                __block uint32_t status = 1;
                dispatch_sync(dispatch_get_main_queue(), ^{
                    HDMacNativeDisplayHost* mainSelf = weakSelf;
                    if (mainSelf == nil) {
                        return;
                    }
                    mainSelf.selectedScanout = select.scanout_id;
                    mainSelf.displayView.guestWidth = MAX(select.guest_width, 1);
                    mainSelf.displayView.guestHeight = MAX(select.guest_height, 1);
                    // gfxstream keeps one CAMetalLayer/CAContext and composes the selected Guest
                    // display into it. Forward the scanout selection to crosvm over the primary
                    // zero-copy connection instead of creating another AppKit window.
                    HDCaConnection* connection =
                        (HDCaConnection*)mainSelf.connections[@0];
                    if (connection == nil || connection.fd < 0) {
                        return;
                    }
                    HDInputEvent event = {
                        .kind = HD_INPUT_SELECT_DISPLAY,
                        .code = (int32_t)select.scanout_id,
                    };
                    const uint8_t* cursor = (const uint8_t*)&event;
                    size_t remaining = sizeof(event);
                    while (remaining != 0) {
                        ssize_t count = send(connection.fd, cursor, remaining, 0);
                        if (count < 0 && errno == EINTR) {
                            continue;
                        }
                        if (count <= 0) {
                            return;
                        }
                        cursor += count;
                        remaining -= (size_t)count;
                    }
                    mainSelf.displayView.clientFD = connection.fd;
                    [mainSelf layoutHostedLayer];
                    status = 0;
                });
                HDCaSelectResponse response = {.magic = HD_CA_SELECT_MAGIC, .status = status};
                (void)send(client, &response, sizeof(response), 0);
                close(client);
                continue;
            }
            uint32_t scanoutId = 0;
            uint32_t contextId = 0;
            uint32_t guestWidth = 0;
            uint32_t guestHeight = 0;
            if (magic == HD_CA_HELLO_MAGIC) {
                HDCaHello hello = {.magic = magic};
                if (!hd_read_full(client, &hello.context_id,
                                  sizeof(hello) - sizeof(hello.magic))) {
                    close(client);
                    continue;
                }
                contextId = hello.context_id;
                guestWidth = hello.guest_width;
                guestHeight = hello.guest_height;
            } else if (magic == HD_CA_HELLO_V2_MAGIC) {
                HDCaHelloV2 hello = {.magic = magic};
                if (!hd_read_full(client, &hello.scanout_id,
                                  sizeof(hello) - sizeof(hello.magic))) {
                    close(client);
                    continue;
                }
                scanoutId = hello.scanout_id;
                contextId = hello.context_id;
                guestWidth = hello.guest_width;
                guestHeight = hello.guest_height;
            } else {
                close(client);
                continue;
            }
            if (contextId == 0) {
                close(client);
                continue;
            }
            dispatch_async(dispatch_get_main_queue(), ^{
                HDMacNativeDisplayHost* mainSelf = weakSelf;
                if (mainSelf == nil) {
                    close(client);
                    return;
                }
                HDCaConnection* previous =
                    (HDCaConnection*)mainSelf.connections[@(scanoutId)];
                if (previous != nil && previous.fd >= 0 && previous.fd != client) {
                    shutdown(previous.fd, SHUT_RDWR);
                    close(previous.fd);
                }
                HDCaConnection* connection = [[HDCaConnection alloc] init];
                connection.fd = client;
                connection.contextId = contextId;
                connection.guestWidth = MAX(guestWidth, 1);
                connection.guestHeight = MAX(guestHeight, 1);
                mainSelf.connections[@(scanoutId)] = connection;
                if (mainSelf.selectedScanout != scanoutId) {
                    return;
                }
                mainSelf.layerHost.contextId = contextId;
                mainSelf.displayView.clientFD = client;
                mainSelf.displayView.guestWidth = MAX(guestWidth, 1);
                mainSelf.displayView.guestHeight = MAX(guestHeight, 1);
                mainSelf.layerHost.contentsScale =
                    mainSelf.parentView.window.backingScaleFactor ?: 1.0;
                [mainSelf layoutHostedLayer];
            });
        }
    });
    return self;
}

- (BOOL)setX:(int32_t)x
           y:(int32_t)y
       width:(uint32_t)width
      height:(uint32_t)height
    rotation:(uint8_t)rotation
     visible:(BOOL)visible {
    NSView* parent = self.parentView;
    if (parent == nil) {
        return NO;
    }
    // SurfaceLayout uses winit physical pixels, while an AppKit NSView frame is
    // expressed in logical points. Convert at the native boundary so the Player
    // host is not twice the visible rectangle on Retina displays.
    CGFloat scale = MAX(parent.window.backingScaleFactor, 1.0);
    NSRect frame = NSMakeRect((CGFloat)x / scale,
                              (CGFloat)y / scale,
                              MAX((CGFloat)width / scale, 1.0),
                              MAX((CGFloat)height / scale, 1.0));
    self.displayView.frame = frame;
    self.displayView.displayRotation = rotation & 3;
    self.displayView.hidden = !visible;
    [self layoutHostedLayer];
    return YES;
}

- (void)layoutHostedLayer {
    if (self.displayView == nil || self.layerHost == nil || self.containerLayer == nil) {
        return;
    }
    NSRect bounds = self.displayView.bounds;
    CGFloat viewWidth = MAX(NSWidth(bounds), 1.0);
    CGFloat viewHeight = MAX(NSHeight(bounds), 1.0);
    CGFloat guestWidth = MAX((CGFloat)self.displayView.guestWidth, 1.0);
    CGFloat guestHeight = MAX((CGFloat)self.displayView.guestHeight, 1.0);
    uint8_t rotation = self.displayView.displayRotation & 3;
    BOOL swapsAxes = (rotation & 1) != 0;
    CGFloat orientedWidth = swapsAxes ? guestHeight : guestWidth;
    CGFloat orientedHeight = swapsAxes ? guestWidth : guestHeight;
    CGFloat renderScale = MIN(viewWidth / orientedWidth, viewHeight / orientedHeight);
    CGFloat renderWidth = orientedWidth * renderScale;
    CGFloat renderHeight = orientedHeight * renderScale;
    CGFloat renderX = (viewWidth - renderWidth) / 2.0;
    CGFloat renderY = (viewHeight - renderHeight) / 2.0;
    CGAffineTransform transform;
    switch (rotation) {
        case 1:
            transform =
                CGAffineTransformMake(0.0, -renderScale, renderScale, 0.0, 0.0, 0.0);
            break;
        case 2:
            transform =
                CGAffineTransformMake(-renderScale, 0.0, 0.0, -renderScale, 0.0, 0.0);
            break;
        case 3:
            transform =
                CGAffineTransformMake(0.0, renderScale, -renderScale, 0.0, 0.0, 0.0);
            break;
        default:
            transform = CGAffineTransformMakeScale(renderScale, renderScale);
            break;
    }
    [CATransaction begin];
    [CATransaction setDisableActions:YES];
    self.containerLayer.frame = bounds;
    // Preserve the Android surface aspect ratio even while AppKit is delivering
    // intermediate resize frames. Any sub-pixel remainder stays black.
    self.layerHost.anchorPoint = CGPointMake(0.5, 0.5);
    self.layerHost.bounds = CGRectMake(0, 0, guestWidth, guestHeight);
    self.layerHost.position = CGPointMake(viewWidth / 2.0, viewHeight / 2.0);
    self.layerHost.affineTransform = transform;
    self.layerHost.contentsScale = self.parentView.window.backingScaleFactor ?: 1.0;
    [CATransaction commit];
    self.displayView.renderX = renderX;
    self.displayView.renderY = renderY;
    self.displayView.renderWidth = renderWidth;
    self.displayView.renderHeight = renderHeight;
}

- (void)shutdown {
    self.displayView.clientFD = -1;
    for (HDCaConnection* connection in self.connections.allValues) {
        if (connection.fd >= 0) {
            shutdown(connection.fd, SHUT_RDWR);
            close(connection.fd);
            connection.fd = -1;
        }
    }
    [self.connections removeAllObjects];
    int listener = self.listenFD;
    self.listenFD = -1;
    if (listener >= 0) {
        shutdown(listener, SHUT_RDWR);
        close(listener);
    }
    if (self.ownsEndpoint) {
        self.ownsEndpoint = NO;
        (void)unlink(self.endpoint.fileSystemRepresentation);
    }
    int lockFD = self.lockFD;
    self.lockFD = -1;
    if (lockFD >= 0) {
        flock(lockFD, LOCK_UN);
        close(lockFD);
    }
    [self.displayView removeFromSuperview];
}

- (void)dealloc {
    [self shutdown];
}

@end

void* hd_macos_native_display_create(void* parent_view, const char* endpoint) {
    if (parent_view == NULL || endpoint == NULL || ![NSThread isMainThread]) {
        return NULL;
    }
    NSView* parent = (__bridge NSView*)parent_view;
    NSString* path = [NSString stringWithUTF8String:endpoint];
    if (path == nil) {
        return NULL;
    }
    HDMacNativeDisplayHost* host =
        [[HDMacNativeDisplayHost alloc] initWithParent:parent endpoint:path];
    return host == nil ? NULL : (__bridge_retained void*)host;
}

void hd_macos_native_display_destroy(void* raw_host) {
    if (raw_host == NULL) {
        return;
    }
    HDMacNativeDisplayHost* host = (__bridge_transfer HDMacNativeDisplayHost*)raw_host;
    [host shutdown];
}

bool hd_macos_native_display_set_bounds(void* raw_host,
                                        int32_t x_px,
                                        int32_t y_px,
                                        uint32_t width_px,
                                        uint32_t height_px,
                                        uint8_t rotation_quarters,
                                        bool visible) {
    if (raw_host == NULL || ![NSThread isMainThread]) {
        return false;
    }
    HDMacNativeDisplayHost* host = (__bridge HDMacNativeDisplayHost*)raw_host;
    return [host setX:x_px
                    y:y_px
                width:width_px
               height:height_px
             rotation:rotation_quarters
              visible:visible];
}

bool hd_macos_native_display_hide(void* raw_host) {
    if (raw_host == NULL || ![NSThread isMainThread]) {
        return false;
    }
    HDMacNativeDisplayHost* host = (__bridge HDMacNativeDisplayHost*)raw_host;
    host.displayView.hidden = YES;
    return true;
}

int32_t hd_macos_choose_apk_file(char* output, size_t capacity) {
    if (![NSThread isMainThread] || output == NULL || capacity == 0) {
        return -1;
    }
    output[0] = '\0';
    NSOpenPanel* panel = [NSOpenPanel openPanel];
    panel.title = @"选择要安装的 APK";
    panel.prompt = @"选择";
    panel.canChooseFiles = YES;
    panel.canChooseDirectories = NO;
    panel.allowsMultipleSelection = NO;
    panel.resolvesAliases = YES;
    UTType* apkType = [UTType typeWithFilenameExtension:@"apk"];
    if (apkType != nil) {
        panel.allowedContentTypes = @[apkType];
    }
    if ([panel runModal] != NSModalResponseOK) {
        return 0;
    }
    const char* path = panel.URL.fileSystemRepresentation;
    if (path == NULL || strlen(path) >= capacity) {
        return -1;
    }
    strlcpy(output, path, capacity);
    return 1;
}

int32_t hd_macos_choose_location_route_file(char* output, size_t capacity) {
    if (![NSThread isMainThread] || output == NULL || capacity == 0) {
        return -1;
    }
    output[0] = '\0';
    NSOpenPanel* panel = [NSOpenPanel openPanel];
    panel.title = @"选择 GPX 或 KML 路线";
    panel.prompt = @"导入";
    panel.canChooseFiles = YES;
    panel.canChooseDirectories = NO;
    panel.allowsMultipleSelection = NO;
    panel.resolvesAliases = YES;
    NSMutableArray<UTType*>* types = [NSMutableArray array];
    UTType* gpxType = [UTType typeWithFilenameExtension:@"gpx"];
    UTType* kmlType = [UTType typeWithFilenameExtension:@"kml"];
    if (gpxType != nil) {
        [types addObject:gpxType];
    }
    if (kmlType != nil) {
        [types addObject:kmlType];
    }
    if (types.count > 0) {
        panel.allowedContentTypes = types;
    }
    if ([panel runModal] != NSModalResponseOK) {
        return 0;
    }
    const char* path = panel.URL.fileSystemRepresentation;
    if (path == NULL || strlen(path) >= capacity) {
        return -1;
    }
    strlcpy(output, path, capacity);
    return 1;
}

bool hd_macos_native_display_focus(void* raw_host) {
    if (raw_host == NULL || ![NSThread isMainThread]) {
        return false;
    }
    HDMacNativeDisplayHost* host = (__bridge HDMacNativeDisplayHost*)raw_host;
    return [host.displayView.window makeFirstResponder:host.displayView];
}

bool hd_macos_native_display_root_is_minimized(void* raw_host) {
    if (raw_host == NULL || ![NSThread isMainThread]) {
        return false;
    }
    HDMacNativeDisplayHost* host = (__bridge HDMacNativeDisplayHost*)raw_host;
    return host.parentView.window.miniaturized;
}

bool hd_macos_center_traffic_lights(void* parent_view, double titlebar_height) {
    if (parent_view == NULL || ![NSThread isMainThread]) {
        return false;
    }
    NSView* parent = (__bridge NSView*)parent_view;
    NSWindow* window = parent.window;
    if (window == nil) {
        return false;
    }
    NSArray<NSButton*>* buttons = @[
        [window standardWindowButton:NSWindowCloseButton],
        [window standardWindowButton:NSWindowMiniaturizeButton],
        [window standardWindowButton:NSWindowZoomButton],
    ];
    for (NSButton* button in buttons) {
        if (button == nil || button.superview == nil) {
            continue;
        }
        NSRect frame = button.frame;
        CGFloat topHeight = MAX((CGFloat)titlebar_height, frame.size.height);
        CGFloat superviewHeight = NSHeight(button.superview.bounds);
        CGFloat y = 0.0;
        if (superviewHeight <= topHeight + 4.0) {
            y = (superviewHeight - frame.size.height) / 2.0;
        } else {
            y = superviewHeight - topHeight + (topHeight - frame.size.height) / 2.0;
        }
        frame.origin.y = floor(MAX(y, 0.0) + 0.5);
        button.frame = frame;
        button.hidden = NO;
    }
    return true;
}

bool hd_macos_set_window_content_aspect_ratio(void* parent_view,
                                               double width,
                                               double height) {
    if (parent_view == NULL || ![NSThread isMainThread] || width <= 0.0 || height <= 0.0) {
        return false;
    }
    NSView* parent = (__bridge NSView*)parent_view;
    NSWindow* window = parent.window;
    if (window == nil) {
        return false;
    }
    window.contentAspectRatio = NSMakeSize((CGFloat)width, (CGFloat)height);
    return true;
}

bool hd_macos_install_titlebar_controls(void* parent_view,
                                        void* context,
                                        HDTitlebarCallback callback) {
    if (parent_view == NULL || callback == NULL || ![NSThread isMainThread]) {
        return false;
    }
    NSView* parent = (__bridge NSView*)parent_view;
    NSWindow* window = parent.window;
    if (window == nil) {
        return false;
    }

    // The Player restores instances and geometry from HD's versioned state, never from an AppKit
    // window archive. Do not create a second, bundle-scoped restoration source on shutdown.
    window.restorable = NO;

    HDTitlebarControls* target =
        [[HDTitlebarControls alloc] initWithContext:context callback:callback];
    NSStackView* leftStack = [[NSStackView alloc] initWithFrame:NSMakeRect(0, 0, 68, 32)];
    leftStack.orientation = NSUserInterfaceLayoutOrientationHorizontal;
    leftStack.alignment = NSLayoutAttributeCenterY;
    leftStack.spacing = 2.0;
    leftStack.edgeInsets = NSEdgeInsetsMake(0, 6, 0, 6);
    NSButton* sidebarButton = hd_titlebar_contract_button(0, target);
    [leftStack addArrangedSubview:sidebarButton];

    hd_fit_titlebar_stack(leftStack);

    NSTitlebarAccessoryViewController* leftController =
        [[NSTitlebarAccessoryViewController alloc] init];
    leftController.view = leftStack;
    leftController.layoutAttribute = NSLayoutAttributeLeft;
    leftController.fullScreenMinHeight = 32.0;

    // Let AppKit size both titlebar accessories from the visible controls. Fixed-width
    // containers reserve the hidden FPS label and make portrait emulator windows clip or wrap.
    NSStackView* rightStack = [[NSStackView alloc] initWithFrame:NSMakeRect(0, 0, 345, 32)];
    rightStack.orientation = NSUserInterfaceLayoutOrientationHorizontal;
    rightStack.alignment = NSLayoutAttributeCenterY;
    rightStack.spacing = 2.0;
    rightStack.edgeInsets = NSEdgeInsetsMake(0, 4, 0, 4);

    NSTextField* fpsLabel = [NSTextField labelWithString:@"— FPS"];
    fpsLabel.font = [NSFont monospacedDigitSystemFontOfSize:10.5
                                                    weight:NSFontWeightMedium];
    fpsLabel.textColor = NSColor.secondaryLabelColor;
    fpsLabel.alignment = NSTextAlignmentRight;
    fpsLabel.toolTip = @"Android 实时渲染帧率";
    fpsLabel.hidden = YES;
    fpsLabel.translatesAutoresizingMaskIntoConstraints = NO;
    [fpsLabel.widthAnchor constraintEqualToConstant:44.0].active = YES;
    [rightStack addArrangedSubview:fpsLabel];

    NSMutableArray<NSButton*>* actionButtons = [[NSMutableArray alloc] init];
    NSView* actionSeparator = nil;
    for (size_t index = 1; index < hd_macos_titlebar_control_count(); index++) {
        if (index == 8) {
            actionSeparator = hd_titlebar_separator();
            actionSeparator.hidden = YES;
            [rightStack addArrangedSubview:actionSeparator];
        }
        NSButton* button = hd_titlebar_contract_button(index, target);
        button.enabled = NO;
        button.hidden = YES;
        [actionButtons addObject:button];
        [rightStack addArrangedSubview:button];
    }
    hd_fit_titlebar_stack(rightStack);

    NSTitlebarAccessoryViewController* rightController =
        [[NSTitlebarAccessoryViewController alloc] init];
    rightController.view = rightStack;
    rightController.layoutAttribute = NSLayoutAttributeRight;
    rightController.fullScreenMinHeight = 32.0;

    while (window.titlebarAccessoryViewControllers.count > 0) {
        [window removeTitlebarAccessoryViewControllerAtIndex:0];
    }
    window.titleVisibility = NSWindowTitleHidden;
    window.titlebarAppearsTransparent = NO;
    [window addTitlebarAccessoryViewController:leftController];
    [window addTitlebarAccessoryViewController:rightController];

    NSNotificationCenter* notifications = NSNotificationCenter.defaultCenter;
    NSArray* previousObservers =
        objc_getAssociatedObject(window, &HDTitlebarExpansionObserversKey);
    for (id observer in previousObservers) {
        [notifications removeObserver:observer];
    }
    id enteredFullScreen =
        [notifications addObserverForName:NSWindowDidEnterFullScreenNotification
                                   object:window
                                    queue:NSOperationQueue.mainQueue
                               usingBlock:^(__unused NSNotification* notification) {
        callback(context, "{\"command\":\"window_expansion\",\"expanded\":true}");
    }];
    id exitedFullScreen =
        [notifications addObserverForName:NSWindowDidExitFullScreenNotification
                                   object:window
                                    queue:NSOperationQueue.mainQueue
                               usingBlock:^(__unused NSNotification* notification) {
        callback(context, "{\"command\":\"window_expansion\",\"expanded\":false}");
    }];
    NSNotificationCenter* workspaceNotifications =
        NSWorkspace.sharedWorkspace.notificationCenter;
    NSArray* previousWorkspaceObservers =
        objc_getAssociatedObject(window, &HDWorkspaceLifecycleObserversKey);
    for (id observer in previousWorkspaceObservers) {
        [workspaceNotifications removeObserver:observer];
    }
    id willSleep =
        [workspaceNotifications addObserverForName:NSWorkspaceWillSleepNotification
                                            object:nil
                                             queue:NSOperationQueue.mainQueue
                                        usingBlock:^(__unused NSNotification* notification) {
        callback(context, "{\"command\":\"native_lifecycle\",\"state\":\"suspended\"}");
    }];
    id didWake =
        [workspaceNotifications addObserverForName:NSWorkspaceDidWakeNotification
                                            object:nil
                                             queue:NSOperationQueue.mainQueue
                                        usingBlock:^(__unused NSNotification* notification) {
        callback(context, "{\"command\":\"native_lifecycle\",\"state\":\"resumed\"}");
    }];
    objc_setAssociatedObject(window, &HDTitlebarTargetKey, target,
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(window, "HDTitlebarControlsControllers",
                             @[leftController, rightController],
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(window, &HDTitlebarFpsLabelKey, fpsLabel,
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(window, &HDTitlebarSidebarButtonKey, sidebarButton,
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(window, &HDTitlebarActionButtonsKey, actionButtons,
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(window, &HDTitlebarSeparatorKey, actionSeparator,
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(window, &HDTitlebarLeftStackKey, leftStack,
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(window, &HDTitlebarRightStackKey, rightStack,
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(window, &HDTitlebarExpansionObserversKey,
                             @[enteredFullScreen, exitedFullScreen],
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(window, &HDWorkspaceLifecycleObserversKey,
                             @[willSleep, didWake],
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    return true;
}

bool hd_macos_set_titlebar_player_state(void* parent_view,
                                         const char* state_json) {
    if (parent_view == NULL || state_json == NULL || ![NSThread isMainThread]) {
        return false;
    }
    NSView* parent = (__bridge NSView*)parent_view;
    NSWindow* window = parent.window;
    if (window == nil) {
        return false;
    }
    NSData* data = [NSData dataWithBytes:state_json length:strlen(state_json)];
    id decoded = [NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
    if (![decoded isKindOfClass:[NSDictionary class]]) {
        return false;
    }
    NSDictionary* state = (NSDictionary*)decoded;
    NSNumber* androidSelectedValue = state[@"android_selected"];
    NSNumber* controlsVisibleValue = state[@"controls_visible"];
    NSNumber* sidebarVisibleValue = state[@"sidebar_visible"];
    NSNumber* powerEnabledValue = state[@"power_enabled"];
    NSNumber* actionsEnabledValue = state[@"actions_enabled"];
    NSNumber* recordingSupportedValue = state[@"recording_supported"];
    NSNumber* recordingActiveValue = state[@"recording_active"];
    NSNumber* recordingEnabledValue = state[@"recording_enabled"];
    if (![androidSelectedValue isKindOfClass:[NSNumber class]] ||
        ![controlsVisibleValue isKindOfClass:[NSNumber class]] ||
        ![sidebarVisibleValue isKindOfClass:[NSNumber class]] ||
        ![powerEnabledValue isKindOfClass:[NSNumber class]] ||
        ![actionsEnabledValue isKindOfClass:[NSNumber class]] ||
        ![recordingSupportedValue isKindOfClass:[NSNumber class]] ||
        ![recordingActiveValue isKindOfClass:[NSNumber class]] ||
        ![recordingEnabledValue isKindOfClass:[NSNumber class]]) {
        return false;
    }

    NSButton* sidebarButton = objc_getAssociatedObject(window, &HDTitlebarSidebarButtonKey);
    NSArray<NSButton*>* actionButtons =
        objc_getAssociatedObject(window, &HDTitlebarActionButtonsKey);
    NSView* actionSeparator = objc_getAssociatedObject(window, &HDTitlebarSeparatorKey);
    NSStackView* leftStack = objc_getAssociatedObject(window, &HDTitlebarLeftStackKey);
    NSStackView* rightStack = objc_getAssociatedObject(window, &HDTitlebarRightStackKey);
    if (![sidebarButton isKindOfClass:[NSButton class]] ||
        ![actionButtons isKindOfClass:[NSArray class]] ||
        actionButtons.count + 1 != hd_macos_titlebar_control_count() ||
        ![leftStack isKindOfClass:[NSStackView class]] ||
        ![rightStack isKindOfClass:[NSStackView class]]) {
        return false;
    }

    BOOL androidSelected = androidSelectedValue.boolValue;
    BOOL controlsVisible = controlsVisibleValue.boolValue;
    if (controlsVisible && !androidSelected) {
        return false;
    }
    BOOL actionsEnabled = actionsEnabledValue.boolValue;
    BOOL recordingSupported = recordingSupportedValue.boolValue;
    BOOL recordingActive = recordingActiveValue.boolValue;
    BOOL sidebarVisible = sidebarVisibleValue.boolValue;
    sidebarButton.toolTip = sidebarVisible ? @"折叠侧栏" : @"展开侧栏";
    sidebarButton.accessibilityLabel = sidebarButton.toolTip;
    for (NSUInteger buttonIndex = 0; buttonIndex < actionButtons.count; buttonIndex++) {
        NSButton* button = actionButtons[buttonIndex];
        size_t contractIndex = buttonIndex + 1;
        BOOL recordingButton = contractIndex == 6;
        button.hidden = !controlsVisible || (recordingButton && !recordingSupported);
        if (contractIndex == 1) {
            button.enabled = powerEnabledValue.boolValue;
        } else if (recordingButton) {
            button.enabled = recordingEnabledValue.boolValue;
        } else {
            button.enabled = actionsEnabled;
        }
    }
    actionSeparator.hidden = !controlsVisible;

    NSButton* recordingButton = actionButtons[5];
    NSString* recordingSymbol = recordingActive ? @"stop.circle.fill" : @"record.circle";
    NSString* recordingTooltip = recordingActive
        ? @"停止录屏并保存"
        : @"开始录屏（最长 3 分钟）";
    NSString* recordingMessage = recordingActive
        ? @"{\"command\":\"stop_screen_recording\"}"
        : @"{\"command\":\"start_screen_recording\"}";
    recordingButton.toolTip = recordingTooltip;
    objc_setAssociatedObject(recordingButton, &HDTitlebarButtonMessageKey, recordingMessage,
                             OBJC_ASSOCIATION_COPY_NONATOMIC);
    if (@available(macOS 11.0, *)) {
        NSImage* image = [NSImage imageWithSystemSymbolName:recordingSymbol
                                  accessibilityDescription:recordingTooltip];
        if (image != nil) {
            recordingButton.image = image;
        }
    }

    hd_fit_titlebar_stack(leftStack);
    hd_fit_titlebar_stack(rightStack);
    [window.contentView.superview setNeedsLayout:YES];
    [window.contentView.superview layoutSubtreeIfNeeded];
    return true;
}

bool hd_macos_set_titlebar_fps(void* parent_view,
                               bool visible,
                               uint32_t fps_milli) {
    if (parent_view == NULL || ![NSThread isMainThread]) {
        return false;
    }
    NSView* parent = (__bridge NSView*)parent_view;
    NSWindow* window = parent.window;
    if (window == nil) {
        return false;
    }
    id value = objc_getAssociatedObject(window, &HDTitlebarFpsLabelKey);
    if (![value isKindOfClass:[NSTextField class]]) {
        return false;
    }
    NSTextField* label = (NSTextField*)value;
    BOOL visibilityChanged = label.hidden == visible;
    label.hidden = !visible;
    if (visible) {
        label.stringValue = fps_milli == 0
            ? @"— FPS"
            : [NSString stringWithFormat:@"%.1f FPS", (double)fps_milli / 1000.0];
    }
    if (visibilityChanged) {
        NSStackView* rightStack = objc_getAssociatedObject(window, &HDTitlebarRightStackKey);
        hd_fit_titlebar_stack(rightStack);
        [window.contentView.superview setNeedsLayout:YES];
        [window.contentView.superview layoutSubtreeIfNeeded];
    }
    return true;
}
