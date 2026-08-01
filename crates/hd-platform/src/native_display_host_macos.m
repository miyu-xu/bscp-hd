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
    HD_INPUT_KEY = 1,
    HD_INPUT_TOUCH_DOWN = 2,
    HD_INPUT_TOUCH_MOVE = 3,
    HD_INPUT_TOUCH_UP = 4,
    HD_INPUT_VIEWPORT_RESIZE = 5,
};

typedef struct {
    uint32_t magic;
    uint32_t context_id;
    uint32_t guest_width;
    uint32_t guest_height;
} HDCaHello;

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
    {"rectangle.stack", "▢", "最近任务",
     "{\"command\":\"key\",\"key\":\"recent\"}", "right"},
    {"house", "⌂", "主页",
     "{\"command\":\"key\",\"key\":\"home\"}", "right"},
    {"chevron.left", "‹", "返回",
     "{\"command\":\"key\",\"key\":\"back\"}", "right"},
};

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
    [button.widthAnchor constraintEqualToConstant:28.0].active = YES;
    [button.heightAnchor constraintEqualToConstant:28.0].active = YES;
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

@interface HDMacNativeDisplayHost : NSObject
@property(nonatomic, weak) NSView* parentView;
@property(nonatomic, strong) HDNativeDisplayView* displayView;
@property(nonatomic, strong) CALayer* containerLayer;
@property(nonatomic, strong) CALayerHost* layerHost;
@property(nonatomic, copy) NSString* endpoint;
@property(nonatomic) BOOL ownsEndpoint;
@property(atomic) int lockFD;
@property(atomic) int listenFD;
@property(atomic) int clientFD;
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
    self.clientFD = -1;

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
        chmod(path, S_IRUSR | S_IWUSR) != 0 || listen(fd, 1) != 0) {
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
            HDCaHello hello = {0};
            if (!hd_read_full(client, &hello, sizeof(hello)) ||
                hello.magic != HD_CA_HELLO_MAGIC || hello.context_id == 0) {
                close(client);
                continue;
            }
            int previousClient = strongSelf.clientFD;
            strongSelf.clientFD = client;
            if (previousClient >= 0 && previousClient != client) {
                shutdown(previousClient, SHUT_RDWR);
                close(previousClient);
            }
            dispatch_async(dispatch_get_main_queue(), ^{
                HDMacNativeDisplayHost* mainSelf = weakSelf;
                if (mainSelf == nil || mainSelf.clientFD != client) {
                    return;
                }
                mainSelf.layerHost.contextId = hello.context_id;
                mainSelf.displayView.clientFD = client;
                mainSelf.displayView.guestWidth = MAX(hello.guest_width, 1);
                mainSelf.displayView.guestHeight = MAX(hello.guest_height, 1);
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
    CGFloat scaleX = viewWidth / orientedWidth;
    CGFloat scaleY = viewHeight / orientedHeight;
    CGAffineTransform transform;
    switch (rotation) {
        case 1:
            transform = CGAffineTransformMake(0.0, -scaleY, scaleX, 0.0, 0.0, 0.0);
            break;
        case 2:
            transform = CGAffineTransformMake(-scaleX, 0.0, 0.0, -scaleY, 0.0, 0.0);
            break;
        case 3:
            transform = CGAffineTransformMake(0.0, scaleY, -scaleX, 0.0, 0.0, 0.0);
            break;
        default:
            transform = CGAffineTransformMakeScale(scaleX, scaleY);
            break;
    }
    [CATransaction begin];
    [CATransaction setDisableActions:YES];
    self.containerLayer.frame = bounds;
    // The Android surface always owns the complete window content area. Window
    // aspect-ratio constraints keep scaleX and scaleY equal during interactive
    // resize; using both values also avoids transient black bars while AppKit is
    // delivering intermediate layout frames. The guest drawable stays unchanged.
    self.layerHost.anchorPoint = CGPointMake(0.5, 0.5);
    self.layerHost.bounds = CGRectMake(0, 0, guestWidth, guestHeight);
    self.layerHost.position = CGPointMake(viewWidth / 2.0, viewHeight / 2.0);
    self.layerHost.affineTransform = transform;
    self.layerHost.contentsScale = self.parentView.window.backingScaleFactor ?: 1.0;
    [CATransaction commit];
    self.displayView.renderX = 0.0;
    self.displayView.renderY = 0.0;
    self.displayView.renderWidth = viewWidth;
    self.displayView.renderHeight = viewHeight;
}

- (void)shutdown {
    self.displayView.clientFD = -1;
    int client = self.clientFD;
    self.clientFD = -1;
    if (client >= 0) {
        shutdown(client, SHUT_RDWR);
        close(client);
    }
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

    HDTitlebarControls* target =
        [[HDTitlebarControls alloc] initWithContext:context callback:callback];
    NSStackView* leftStack = [[NSStackView alloc] initWithFrame:NSMakeRect(0, 0, 42, 32)];
    leftStack.orientation = NSUserInterfaceLayoutOrientationHorizontal;
    leftStack.alignment = NSLayoutAttributeCenterY;
    leftStack.spacing = 5.0;
    leftStack.edgeInsets = NSEdgeInsetsMake(0, 6, 0, 6);
    [leftStack addArrangedSubview:hd_titlebar_contract_button(0, target)];

    NSView* leftContainer = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, 48, 32)];
    leftStack.translatesAutoresizingMaskIntoConstraints = NO;
    [leftContainer addSubview:leftStack];
    [NSLayoutConstraint activateConstraints:@[
        [leftStack.leadingAnchor constraintEqualToAnchor:leftContainer.leadingAnchor],
        [leftStack.trailingAnchor constraintLessThanOrEqualToAnchor:leftContainer.trailingAnchor],
        [leftStack.centerYAnchor constraintEqualToAnchor:leftContainer.centerYAnchor],
    ]];

    NSTitlebarAccessoryViewController* leftController =
        [[NSTitlebarAccessoryViewController alloc] init];
    leftController.view = leftContainer;
    leftController.layoutAttribute = NSLayoutAttributeLeft;
    leftController.fullScreenMinHeight = 32.0;

    // Keep the accessory at its intrinsic width. A fixed 400pt container forces
    // AppKit to squeeze or wrap titlebar accessories in a portrait emulator
    // window even though the controls themselves need less than 280pt.
    NSStackView* rightStack = [[NSStackView alloc] initWithFrame:NSMakeRect(0, 0, 278, 32)];
    rightStack.orientation = NSUserInterfaceLayoutOrientationHorizontal;
    rightStack.alignment = NSLayoutAttributeCenterY;
    rightStack.spacing = 5.0;
    rightStack.edgeInsets = NSEdgeInsetsMake(0, 6, 0, 6);

    for (size_t index = 1; index < hd_macos_titlebar_control_count(); index++) {
        if (index == 6) {
            [rightStack addArrangedSubview:hd_titlebar_separator()];
        }
        [rightStack addArrangedSubview:hd_titlebar_contract_button(index, target)];
    }

    NSView* rightContainer = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, 284, 32)];
    rightStack.translatesAutoresizingMaskIntoConstraints = NO;
    [rightContainer addSubview:rightStack];
    [NSLayoutConstraint activateConstraints:@[
        [rightStack.trailingAnchor constraintEqualToAnchor:rightContainer.trailingAnchor],
        [rightStack.leadingAnchor constraintGreaterThanOrEqualToAnchor:rightContainer.leadingAnchor],
        [rightStack.centerYAnchor constraintEqualToAnchor:rightContainer.centerYAnchor],
    ]];

    NSTitlebarAccessoryViewController* rightController =
        [[NSTitlebarAccessoryViewController alloc] init];
    rightController.view = rightContainer;
    rightController.layoutAttribute = NSLayoutAttributeRight;
    rightController.fullScreenMinHeight = 32.0;

    while (window.titlebarAccessoryViewControllers.count > 0) {
        [window removeTitlebarAccessoryViewControllerAtIndex:0];
    }
    window.titleVisibility = NSWindowTitleHidden;
    window.titlebarAppearsTransparent = NO;
    [window addTitlebarAccessoryViewController:leftController];
    [window addTitlebarAccessoryViewController:rightController];

    objc_setAssociatedObject(window, "HDTitlebarControlsTarget", target,
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(window, "HDTitlebarControlsControllers",
                             @[leftController, rightController],
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    return true;
}
