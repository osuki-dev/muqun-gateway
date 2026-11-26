#import <Cocoa/Cocoa.h>
#import <WebKit/WebKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <QuartzCore/QuartzCore.h>
#import <ApplicationServices/ApplicationServices.h>
#import <dispatch/dispatch.h>
#import <math.h>
#import <stdint.h>
#import <stdlib.h>
#import <string.h>

typedef struct MuqunMacCallbacks {
    void *ctx;
    int (*allow_navigation)(void *ctx, const char *url);
    void (*on_frame)(void *ctx, uint32_t width, uint32_t height, const uint8_t *rgba, size_t len);
    void (*on_nav)(void *ctx, const char *url, int back, int forward, const char *title, int committed);
    void (*on_browser_icon)(void *ctx, const char *page_url, const char *msg_json);
    void (*on_draft)(void *ctx, const char *page_url, const char *msg_json);
    void (*on_cursor)(void *ctx, const char *cursor_name);
    uint8_t (*get_idle)(void *ctx);
    void (*on_terminated)(void *ctx);
} MuqunMacCallbacks;

@interface MuqunMacSurface : NSObject <WKNavigationDelegate, WKUIDelegate, WKScriptMessageHandler>
@property (nonatomic, assign) MuqunMacCallbacks cb;
@property (nonatomic, assign) BOOL browserMode;
@property (nonatomic, assign) BOOL cursorEnabled;
@property (nonatomic, assign) BOOL visible;
@property (nonatomic, assign) BOOL snapshotPending;
@property (nonatomic, assign) BOOL provisional;
@property (nonatomic, assign) BOOL stopped;
@property (nonatomic, assign) int32_t logicalW;
@property (nonatomic, assign) int32_t logicalH;
@property (nonatomic, assign) int32_t rasterW;
@property (nonatomic, assign) int32_t rasterH;
@property (nonatomic, strong) NSWindow *window;
@property (nonatomic, strong) WKWebView *webView;
@property (nonatomic, strong) WKUserContentController *ucc;
@property (nonatomic, strong) NSTimer *captureTimer;
@property (nonatomic, copy) NSString *contextScript;
@property (nonatomic, copy) NSString *draftScript;
@property (nonatomic, copy) NSString *faviconScript;
@property (nonatomic, copy) NSString *cursorScript;
@property (nonatomic, assign) NSTimeInterval lastClickTime;
@property (nonatomic, assign) NSPoint lastClickPoint;
@property (nonatomic, assign) NSInteger clickCount;
@property (nonatomic, assign) NSUInteger pressedButtons;
- (void)scheduleCaptureWithDelay:(NSTimeInterval)delay;
- (void)captureNow;
- (void)emitNav;
- (void)rebuildUserScripts;
@end

static BOOL MuqunKeyInfo(NSString *name, uint16_t *outCode, NSString **outChars) {
    if (!name || name.length == 0) return NO;
    NSDictionary<NSString *, NSArray *> *special = @{
        @"Return":    @[@36, @"\r"],
        @"BackSpace": @[@51, @"\x7f"],
        @"Tab":       @[@48, @"\t"],
        @"Escape":    @[@53, @"\x1b"],
        @"Delete":    @[@117, [NSString stringWithFormat:@"%C", (unichar)NSDeleteFunctionKey]],
        @"Up":        @[@126, [NSString stringWithFormat:@"%C", (unichar)NSUpArrowFunctionKey]],
        @"Down":      @[@125, [NSString stringWithFormat:@"%C", (unichar)NSDownArrowFunctionKey]],
        @"Left":      @[@123, [NSString stringWithFormat:@"%C", (unichar)NSLeftArrowFunctionKey]],
        @"Right":     @[@124, [NSString stringWithFormat:@"%C", (unichar)NSRightArrowFunctionKey]],
        @"Home":      @[@115, [NSString stringWithFormat:@"%C", (unichar)NSHomeFunctionKey]],
        @"End":       @[@119, [NSString stringWithFormat:@"%C", (unichar)NSEndFunctionKey]],
        @"Page_Up":   @[@116, [NSString stringWithFormat:@"%C", (unichar)NSPageUpFunctionKey]],
        @"Page_Down": @[@121, [NSString stringWithFormat:@"%C", (unichar)NSPageDownFunctionKey]],
        @"Insert":    @[@114, [NSString stringWithFormat:@"%C", (unichar)NSInsertFunctionKey]],
        @"F1":        @[@122, [NSString stringWithFormat:@"%C", (unichar)NSF1FunctionKey]],
        @"F2":        @[@120, [NSString stringWithFormat:@"%C", (unichar)NSF2FunctionKey]],
        @"F3":        @[@99,  [NSString stringWithFormat:@"%C", (unichar)NSF3FunctionKey]],
        @"F4":        @[@118, [NSString stringWithFormat:@"%C", (unichar)NSF4FunctionKey]],
        @"F5":        @[@96,  [NSString stringWithFormat:@"%C", (unichar)NSF5FunctionKey]],
        @"F6":        @[@97,  [NSString stringWithFormat:@"%C", (unichar)NSF6FunctionKey]],
        @"F7":        @[@98,  [NSString stringWithFormat:@"%C", (unichar)NSF7FunctionKey]],
        @"F8":        @[@100, [NSString stringWithFormat:@"%C", (unichar)NSF8FunctionKey]],
        @"F9":        @[@101, [NSString stringWithFormat:@"%C", (unichar)NSF9FunctionKey]],
        @"F10":       @[@109, [NSString stringWithFormat:@"%C", (unichar)NSF10FunctionKey]],
        @"F11":       @[@103, [NSString stringWithFormat:@"%C", (unichar)NSF11FunctionKey]],
        @"F12":       @[@111, [NSString stringWithFormat:@"%C", (unichar)NSF12FunctionKey]],
    };
    NSArray *hit = special[name];
    if (hit) {
        *outCode = (uint16_t)[hit[0] unsignedShortValue];
        *outChars = hit[1];
        return YES;
    }
    *outCode = 0;
    *outChars = name;
    return YES;
}

static NSString *MuqunNSString(const char *s) {
    if (!s) return @"";
    NSString *out = [NSString stringWithUTF8String:s];
    return out ? out : @"";
}

@implementation MuqunMacSurface

- (void)rebuildUserScripts {
    [self.ucc removeAllUserScripts];
    if (self.contextScript.length > 0) {
        WKUserScript *script = [[WKUserScript alloc]
            initWithSource:self.contextScript
            injectionTime:WKUserScriptInjectionTimeAtDocumentStart
            forMainFrameOnly:YES];
        [self.ucc addUserScript:script];
    }
    if (self.draftScript.length > 0) {
        WKUserScript *script = [[WKUserScript alloc]
            initWithSource:self.draftScript
            injectionTime:WKUserScriptInjectionTimeAtDocumentStart
            forMainFrameOnly:YES];
        [self.ucc addUserScript:script];
    }
    if (self.faviconScript.length > 0) {
        WKUserScript *script = [[WKUserScript alloc]
            initWithSource:self.faviconScript
            injectionTime:WKUserScriptInjectionTimeAtDocumentEnd
            forMainFrameOnly:YES];
        [self.ucc addUserScript:script];
    }
    if (self.cursorScript.length > 0) {
        WKUserScript *script = [[WKUserScript alloc]
            initWithSource:self.cursorScript
            injectionTime:WKUserScriptInjectionTimeAtDocumentEnd
            forMainFrameOnly:YES];
        [self.ucc addUserScript:script];
    }
}

- (void)emitNav {
    if (!self.browserMode || self.stopped) return;
    NSString *urlStr = self.webView.URL.absoluteString ?: @"";
    NSString *titleStr = self.webView.title ?: @"";
    self.cb.on_nav(
        self.cb.ctx,
        urlStr.UTF8String,
        self.webView.canGoBack ? 1 : 0,
        self.webView.canGoForward ? 1 : 0,
        titleStr.UTF8String,
        self.provisional ? 0 : 1
    );
}

- (void)observeValueForKeyPath:(NSString *)keyPath
                      ofObject:(id)object
                        change:(NSDictionary<NSKeyValueChangeKey,id> *)change
                       context:(void *)context {
    if (object == self.webView) {
        [self emitNav];
        return;
    }
    [super observeValueForKeyPath:keyPath ofObject:object change:change context:context];
}

- (void)webView:(WKWebView *)webView
    decidePolicyForNavigationAction:(WKNavigationAction *)navigationAction
    decisionHandler:(void (^)(WKNavigationActionPolicy))decisionHandler {
    NSString *urlStr = navigationAction.request.URL.absoluteString ?: @"";
    if ([urlStr isEqualToString:@"about:blank"] || [urlStr isEqualToString:@"about:srcdoc"]) {
        decisionHandler(WKNavigationActionPolicyAllow);
        return;
    }
    int ok = self.cb.allow_navigation(self.cb.ctx, urlStr.UTF8String);
    decisionHandler(ok ? WKNavigationActionPolicyAllow : WKNavigationActionPolicyCancel);
}

- (WKWebView *)webView:(WKWebView *)webView
    createWebViewWithConfiguration:(WKWebViewConfiguration *)configuration
    forNavigationAction:(WKNavigationAction *)navigationAction
    windowFeatures:(WKWindowFeatures *)windowFeatures {
    // Deny popups, matching Linux NewWindowResponse::Deny.
    return nil;
}

- (void)webView:(WKWebView *)webView didStartProvisionalNavigation:(WKNavigation *)navigation {
    self.provisional = YES;
    [self emitNav];
}

- (void)webView:(WKWebView *)webView didCommitNavigation:(WKNavigation *)navigation {
    self.provisional = NO;
    [self emitNav];
    [self scheduleCaptureWithDelay:0.0];
}

- (void)webView:(WKWebView *)webView didFinishNavigation:(WKNavigation *)navigation {
    self.provisional = NO;
    [self emitNav];
    [self scheduleCaptureWithDelay:0.0];
}

- (void)webViewWebContentProcessDidTerminate:(WKWebView *)webView {
    if (self.cb.on_terminated) {
        self.cb.on_terminated(self.cb.ctx);
    }
    self.stopped = YES;
    [NSApp stop:nil];
    NSEvent *wakeup = [NSEvent otherEventWithType:NSEventTypeApplicationDefined
                                         location:NSZeroPoint
                                    modifierFlags:0
                                        timestamp:0
                                     windowNumber:0
                                          context:nil
                                          subtype:0
                                            data1:0
                                            data2:0];
    [NSApp postEvent:wakeup atStart:YES];
}

- (void)userContentController:(WKUserContentController *)userContentController
      didReceiveScriptMessage:(WKScriptMessage *)message {
    if (self.stopped) return;
    NSString *pageUrl = self.webView.URL.absoluteString ?: @"";
    if ([message.name isEqualToString:@"muqunDraft"] && [message.body isKindOfClass:[NSString class]]) {
        self.cb.on_draft(self.cb.ctx, pageUrl.UTF8String, ((NSString *)message.body).UTF8String);
    } else if ([message.name isEqualToString:@"browserIcon"] && [message.body isKindOfClass:[NSString class]]) {
        self.cb.on_browser_icon(self.cb.ctx, pageUrl.UTF8String, ((NSString *)message.body).UTF8String);
    } else if ([message.name isEqualToString:@"muqunCursor"] && [message.body isKindOfClass:[NSString class]]) {
        self.cb.on_cursor(self.cb.ctx, ((NSString *)message.body).UTF8String);
    }
}

- (void)scheduleCaptureWithDelay:(NSTimeInterval)delay {
    if (self.stopped || !self.visible || self.snapshotPending || self.captureTimer != nil) {
        return;
    }
    __weak typeof(self) weakSelf = self;
    self.captureTimer = [NSTimer scheduledTimerWithTimeInterval:delay
                                                        repeats:NO
                                                          block:^(NSTimer * _Nonnull timer) {
        __strong typeof(weakSelf) strongSelf = weakSelf;
        if (!strongSelf) return;
        strongSelf.captureTimer = nil;
        [strongSelf captureNow];
    }];
}

- (void)wakeCapture {
    if (self.stopped || !self.visible) return;
    if (self.captureTimer) {
        [self.captureTimer invalidate];
        self.captureTimer = nil;
    }
    [self scheduleCaptureWithDelay:0.0];
}

- (void)captureNow {
    if (self.stopped || !self.visible || self.snapshotPending) return;
    self.snapshotPending = YES;
    NSDate *started = [NSDate date];
    int32_t targetW = self.rasterW;
    int32_t targetH = self.rasterH;
    int32_t logW = self.logicalW;
    int32_t logH = self.logicalH;

    WKSnapshotConfiguration *cfg = [[WKSnapshotConfiguration alloc] init];
    cfg.rect = CGRectMake(0, 0, (CGFloat)logW, (CGFloat)logH);
    cfg.afterScreenUpdates = NO;

    __weak typeof(self) weakSelf = self;
    [self.webView takeSnapshotWithConfiguration:cfg
                              completionHandler:^(NSImage * _Nullable image, NSError * _Nullable error) {
        __strong typeof(weakSelf) s = weakSelf;
        if (!s) return;
        s.snapshotPending = NO;
        if (s.stopped || !s.visible) return;

        if (image && !error && s.rasterW == targetW && s.rasterH == targetH && targetW > 0 && targetH > 0) {
            NSRect rect = NSMakeRect(0, 0, image.size.width, image.size.height);
            CGImageRef cgImg = [image CGImageForProposedRect:&rect context:nil hints:nil];
            if (cgImg) {
                size_t byteLen = (size_t)targetW * (size_t)targetH * 4;
                uint8_t *buf = (uint8_t *)calloc(1, byteLen);
                if (buf) {
                    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
                    CGContextRef ctx = CGBitmapContextCreate(
                        buf,
                        (size_t)targetW,
                        (size_t)targetH,
                        8,
                        (size_t)targetW * 4,
                        cs,
                        kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big
                    );
                    if (ctx) {
                        CGContextSetInterpolationQuality(ctx, kCGInterpolationMedium);
                        CGContextDrawImage(ctx, CGRectMake(0, 0, (CGFloat)targetW, (CGFloat)targetH), cgImg);
                        CGContextRelease(ctx);
                        // Unpremultiply to straight RGBA expected by Rune Pixels.
                        for (size_t i = 0; i < byteLen; i += 4) {
                            uint8_t a = buf[i + 3];
                            if (a == 0) {
                                buf[i] = 0; buf[i + 1] = 0; buf[i + 2] = 0;
                            } else if (a < 255) {
                                uint16_t alpha = (uint16_t)a;
                                buf[i]     = (uint8_t)(((uint16_t)buf[i]     * 255 + alpha / 2) / alpha);
                                buf[i + 1] = (uint8_t)(((uint16_t)buf[i + 1] * 255 + alpha / 2) / alpha);
                                buf[i + 2] = (uint8_t)(((uint16_t)buf[i + 2] * 255 + alpha / 2) / alpha);
                            }
                        }
                        s.cb.on_frame(s.cb.ctx, (uint32_t)targetW, (uint32_t)targetH, buf, byteLen);
                    }
                    CGColorSpaceRelease(cs);
                    free(buf);
                }
            }
        }

        uint8_t idleCount = s.cb.get_idle ? s.cb.get_idle(s.cb.ctx) : 0;
        double intervalMs;
        if (idleCount <= 3) {
            uint64_t pixels = (uint64_t)targetW * (uint64_t)targetH;
            double calc = (double)(pixels * 1000ULL / 90000000ULL);
            if (calc < 16.0) calc = 16.0;
            if (calc > 160.0) calc = 160.0;
            intervalMs = calc;
        } else if (idleCount <= 7) {
            intervalMs = 240.0;
        } else {
            intervalMs = 800.0;
        }
        double elapsedMs = -[started timeIntervalSinceNow] * 1000.0;
        double delaySec = (intervalMs - elapsedMs) / 1000.0;
        if (delaySec < 0.0) delaySec = 0.0;
        [s scheduleCaptureWithDelay:delaySec];
    }];
}

- (void)resizeLogicalW:(int32_t)lw logicalH:(int32_t)lh rasterW:(int32_t)rw rasterH:(int32_t)rh {
    self.logicalW = lw;
    self.logicalH = lh;
    self.rasterW = rw;
    self.rasterH = rh;
    NSRect winFrame = NSMakeRect(-16000.0, -16000.0, (CGFloat)lw, (CGFloat)lh);
    [self.window setFrame:winFrame display:NO];
    [self.webView setFrame:NSMakeRect(0.0, 0.0, (CGFloat)lw, (CGFloat)lh)];
    [self wakeCapture];
}

- (void)setVisibleFlag:(BOOL)show {
    self.visible = show;
    if (self.captureTimer) {
        [self.captureTimer invalidate];
        self.captureTimer = nil;
    }
    if (show) {
        [self scheduleCaptureWithDelay:0.0];
    } else {
        [self.window makeFirstResponder:nil];
    }
}

- (void)setFocusFlag:(BOOL)focused {
    if (focused) {
        [self.window makeFirstResponder:self.webView];
    } else {
        [self.window makeFirstResponder:nil];
    }
}

- (void)forwardPointerPhase:(int)phase x:(double)x y:(double)y button:(int)button mods:(NSUInteger)mods {
    if (self.stopped) return;
    CGFloat flipY = (CGFloat)self.logicalH - (CGFloat)y;
    NSPoint loc = NSMakePoint((CGFloat)x, flipY);
    NSTimeInterval ts = [[NSProcessInfo processInfo] systemUptime];
    NSView *target = [self.webView hitTest:loc] ?: self.webView;

    NSEventType type;
    NSInteger clicks = 0;
    if (phase == 0) { // down
        [self.window makeFirstResponder:self.webView];
        if (ts - self.lastClickTime < 0.35 && hypot(loc.x - self.lastClickPoint.x, loc.y - self.lastClickPoint.y) <= 4.0) {
            self.clickCount = MIN(self.clickCount + 1, 3);
        } else {
            self.clickCount = 1;
        }
        self.lastClickTime = ts;
        self.lastClickPoint = loc;
        clicks = self.clickCount;
        if (button == 3) {
            type = NSEventTypeRightMouseDown;
            self.pressedButtons |= 2;
        } else if (button == 2) {
            type = NSEventTypeOtherMouseDown;
            self.pressedButtons |= 4;
        } else {
            type = NSEventTypeLeftMouseDown;
            self.pressedButtons |= 1;
        }
    } else if (phase == 1) { // up
        clicks = MAX(self.clickCount, 1);
        if (button == 3) {
            type = NSEventTypeRightMouseUp;
            self.pressedButtons &= ~((NSUInteger)2);
        } else if (button == 2) {
            type = NSEventTypeOtherMouseUp;
            self.pressedButtons &= ~((NSUInteger)4);
        } else {
            type = NSEventTypeLeftMouseUp;
            self.pressedButtons &= ~((NSUInteger)1);
        }
    } else { // move
        if (self.pressedButtons & 1) {
            type = NSEventTypeLeftMouseDragged;
        } else if (self.pressedButtons & 2) {
            type = NSEventTypeRightMouseDragged;
        } else if (self.pressedButtons & 4) {
            type = NSEventTypeOtherMouseDragged;
        } else {
            type = NSEventTypeMouseMoved;
        }
    }

    NSEvent *ev = [NSEvent mouseEventWithType:type
                                     location:loc
                                modifierFlags:mods
                                    timestamp:ts
                                 windowNumber:self.window.windowNumber
                                      context:nil
                                  eventNumber:0
                                   clickCount:clicks
                                     pressure:(phase == 0 ? 1.0f : 0.0f)];
    if (!ev) return;
    switch (type) {
        case NSEventTypeLeftMouseDown:     [target mouseDown:ev]; break;
        case NSEventTypeLeftMouseUp:       [target mouseUp:ev]; break;
        case NSEventTypeLeftMouseDragged:  [target mouseDragged:ev]; break;
        case NSEventTypeRightMouseDown:    [target rightMouseDown:ev]; break;
        case NSEventTypeRightMouseUp:      [target rightMouseUp:ev]; break;
        case NSEventTypeRightMouseDragged: [target rightMouseDragged:ev]; break;
        case NSEventTypeOtherMouseDown:    [target otherMouseDown:ev]; break;
        case NSEventTypeOtherMouseUp:      [target otherMouseUp:ev]; break;
        case NSEventTypeOtherMouseDragged: [target otherMouseDragged:ev]; break;
        case NSEventTypeMouseMoved:        [target mouseMoved:ev]; break;
        default: break;
    }
    [self wakeCapture];
}

- (void)forwardWheelX:(double)x y:(double)y dx:(double)dx dy:(double)dy mods:(NSUInteger)mods {
    if (self.stopped) return;
    CGFloat flipY = (CGFloat)self.logicalH - (CGFloat)y;
    NSPoint loc = NSMakePoint((CGFloat)x, flipY);
    NSView *target = [self.webView hitTest:loc] ?: self.webView;

    int32_t wheelY = (int32_t)llround(-dy);
    int32_t wheelX = (int32_t)llround(-dx);
    CGEventRef cgEv = CGEventCreateScrollWheelEvent(NULL, kCGScrollEventUnitPixel, 2, wheelY, wheelX);
    if (cgEv) {
        CGEventSetLocation(cgEv, CGPointMake(loc.x, loc.y));
        if (mods != 0) {
            CGEventFlags flags = 0;
            if (mods & NSEventModifierFlagShift)   flags |= kCGEventFlagMaskShift;
            if (mods & NSEventModifierFlagControl) flags |= kCGEventFlagMaskControl;
            if (mods & NSEventModifierFlagOption)  flags |= kCGEventFlagMaskAlternate;
            if (mods & NSEventModifierFlagCommand) flags |= kCGEventFlagMaskCommand;
            CGEventSetFlags(cgEv, flags);
        }
        NSEvent *nsEv = [NSEvent eventWithCGEvent:cgEv];
        CFRelease(cgEv);
        if (nsEv) {
            [target scrollWheel:nsEv];
        }
    }
    [self wakeCapture];
}

- (void)forwardKey:(NSString *)name down:(BOOL)down mods:(NSUInteger)mods {
    if (self.stopped || name.length == 0) return;
    // Standard macOS Cmd editing shortcuts when Command is held.
    if (down && (mods & NSEventModifierFlagCommand) && !(mods & NSEventModifierFlagControl)) {
        NSString *lower = name.lowercaseString;
        if ([lower isEqualToString:@"c"]) { [NSApp sendAction:@selector(copy:) to:nil from:nil]; return; }
        if ([lower isEqualToString:@"v"]) { [NSApp sendAction:@selector(paste:) to:nil from:nil]; [self wakeCapture]; return; }
        if ([lower isEqualToString:@"x"]) { [NSApp sendAction:@selector(cut:) to:nil from:nil]; [self wakeCapture]; return; }
        if ([lower isEqualToString:@"a"]) { [NSApp sendAction:@selector(selectAll:) to:nil from:nil]; [self wakeCapture]; return; }
        if ([lower isEqualToString:@"z"]) {
            SEL sel = (mods & NSEventModifierFlagShift) ? @selector(redo:) : @selector(undo:);
            [NSApp sendAction:sel to:nil from:nil];
            [self wakeCapture];
            return;
        }
    }
    uint16_t keyCode = 0;
    NSString *chars = nil;
    if (!MuqunKeyInfo(name, &keyCode, &chars)) return;
    NSTimeInterval ts = [[NSProcessInfo processInfo] systemUptime];
    NSEventType type = down ? NSEventTypeKeyDown : NSEventTypeKeyUp;
    NSEvent *ev = [NSEvent keyEventWithType:type
                                   location:NSZeroPoint
                              modifierFlags:mods
                                  timestamp:ts
                               windowNumber:self.window.windowNumber
                                    context:nil
                                 characters:chars
                charactersIgnoringModifiers:chars
                                  isARepeat:NO
                                    keyCode:keyCode];
    if (!ev) return;
    NSResponder *resp = self.window.firstResponder ?: self.webView;
    if (down) {
        [resp keyDown:ev];
    } else {
        [resp keyUp:ev];
    }
    [self wakeCapture];
}

@end

static MuqunMacSurface *gSurface = nil;

void muqun_macos_run(
    const char *url,
    int browser_mode,
    int cursor_enabled,
    int32_t logical_w,
    int32_t logical_h,
    int32_t raster_w,
    int32_t raster_h,
    const char *context_js,
    const char *draft_js,
    const char *favicon_js,
    const char *cursor_js,
    MuqunMacCallbacks cb
) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyProhibited];

        MuqunMacSurface *surface = [[MuqunMacSurface alloc] init];
        surface.cb = cb;
        surface.browserMode = browser_mode != 0;
        surface.cursorEnabled = cursor_enabled != 0;
        surface.visible = YES;
        surface.provisional = YES;
        surface.logicalW = logical_w;
        surface.logicalH = logical_h;
        surface.rasterW = raster_w;
        surface.rasterH = raster_h;
        if (context_js && context_js[0]) surface.contextScript = MuqunNSString(context_js);
        if (draft_js && draft_js[0]) surface.draftScript = MuqunNSString(draft_js);
        if (favicon_js && favicon_js[0]) surface.faviconScript = MuqunNSString(favicon_js);
        if (cursor_js && cursor_js[0]) surface.cursorScript = MuqunNSString(cursor_js);

        WKWebViewConfiguration *config = [[WKWebViewConfiguration alloc] init];
        config.websiteDataStore = [WKWebsiteDataStore nonPersistentDataStore];
        WKUserContentController *ucc = [[WKUserContentController alloc] init];
        surface.ucc = ucc;
        if (surface.draftScript.length > 0) {
            [ucc addScriptMessageHandler:surface name:@"muqunDraft"];
        }
        if (surface.browserMode) {
            [ucc addScriptMessageHandler:surface name:@"browserIcon"];
        }
        if (surface.cursorEnabled) {
            [ucc addScriptMessageHandler:surface name:@"muqunCursor"];
        }
        [surface rebuildUserScripts];
        config.userContentController = ucc;

        NSRect winRect = NSMakeRect(-16000.0, -16000.0, (CGFloat)logical_w, (CGFloat)logical_h);
        NSWindow *window = [[NSWindow alloc]
            initWithContentRect:winRect
                      styleMask:NSWindowStyleMaskBorderless
                        backing:NSBackingStoreBuffered
                          defer:NO];
        [window setOpaque:NO];
        [window setHasShadow:NO];
        [window setIgnoresMouseEvents:YES];
        [window setCollectionBehavior:
            NSWindowCollectionBehaviorCanJoinAllSpaces |
            NSWindowCollectionBehaviorStationary |
            NSWindowCollectionBehaviorIgnoresCycle];
        [window setAlphaValue:1.0];
        [window setReleasedWhenClosed:NO];

        WKWebView *webView = [[WKWebView alloc]
            initWithFrame:NSMakeRect(0.0, 0.0, (CGFloat)logical_w, (CGFloat)logical_h)
            configuration:config];
        webView.navigationDelegate = surface;
        webView.UIDelegate = surface;
        [window.contentView setWantsLayer:YES];
        [window.contentView addSubview:webView];
        [window orderBack:nil];

        surface.window = window;
        surface.webView = webView;
        gSurface = surface;

        if (surface.browserMode) {
            [webView addObserver:surface forKeyPath:@"URL" options:NSKeyValueObservingOptionNew context:NULL];
            [webView addObserver:surface forKeyPath:@"title" options:NSKeyValueObservingOptionNew context:NULL];
            [webView addObserver:surface forKeyPath:@"canGoBack" options:NSKeyValueObservingOptionNew context:NULL];
            [webView addObserver:surface forKeyPath:@"canGoForward" options:NSKeyValueObservingOptionNew context:NULL];
        }

        NSURL *nsUrl = [NSURL URLWithString:MuqunNSString(url)];
        if (nsUrl) {
            [webView loadRequest:[NSURLRequest requestWithURL:nsUrl]];
        }
        [surface scheduleCaptureWithDelay:0.05];

        [NSApp run];

        surface.stopped = YES;
        if (surface.captureTimer) {
            [surface.captureTimer invalidate];
            surface.captureTimer = nil;
        }
        if (surface.browserMode) {
            @try {
                [webView removeObserver:surface forKeyPath:@"URL"];
                [webView removeObserver:surface forKeyPath:@"title"];
                [webView removeObserver:surface forKeyPath:@"canGoBack"];
                [webView removeObserver:surface forKeyPath:@"canGoForward"];
            } @catch (__unused NSException *e) {}
        }
        [ucc removeScriptMessageHandlerForName:@"muqunDraft"];
        [ucc removeScriptMessageHandlerForName:@"browserIcon"];
        [ucc removeScriptMessageHandlerForName:@"muqunCursor"];
        webView.navigationDelegate = nil;
        webView.UIDelegate = nil;
        [window close];
        gSurface = nil;
    }
}

void muqun_macos_resize(int32_t lw, int32_t lh, int32_t rw, int32_t rh) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (gSurface && !gSurface.stopped) {
            [gSurface resizeLogicalW:lw logicalH:lh rasterW:rw rasterH:rh];
        }
    });
}

void muqun_macos_focus(int focused) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (gSurface && !gSurface.stopped) {
            [gSurface setFocusFlag:focused != 0];
        }
    });
}

void muqun_macos_visible(int show) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (gSurface && !gSurface.stopped) {
            [gSurface setVisibleFlag:show != 0];
        }
    });
}

void muqun_macos_navigate(const char *url) {
    NSString *s = MuqunNSString(url);
    dispatch_async(dispatch_get_main_queue(), ^{
        if (gSurface && !gSurface.stopped) {
            NSURL *u = [NSURL URLWithString:s];
            if (u) [gSurface.webView loadRequest:[NSURLRequest requestWithURL:u]];
        }
    });
}

void muqun_macos_history(int action) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (!gSurface || gSurface.stopped) return;
        if (action == 0) [gSurface.webView goBack];
        else if (action == 1) [gSurface.webView goForward];
        else [gSurface.webView reload];
        [gSurface wakeCapture];
    });
}

void muqun_macos_eval_js(const char *script) {
    NSString *s = MuqunNSString(script);
    dispatch_async(dispatch_get_main_queue(), ^{
        if (gSurface && !gSurface.stopped && s.length > 0) {
            [gSurface.webView evaluateJavaScript:s completionHandler:nil];
        }
    });
}

void muqun_macos_set_context(const char *script) {
    NSString *s = MuqunNSString(script);
    dispatch_async(dispatch_get_main_queue(), ^{
        if (gSurface && !gSurface.stopped) {
            gSurface.contextScript = s;
            [gSurface rebuildUserScripts];
            if (s.length > 0) {
                [gSurface.webView evaluateJavaScript:s completionHandler:nil];
            }
        }
    });
}

void muqun_macos_pointer(int phase, double x, double y, int button, uint32_t mods) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (gSurface && !gSurface.stopped) {
            [gSurface forwardPointerPhase:phase x:x y:y button:button mods:(NSUInteger)mods];
        }
    });
}

void muqun_macos_wheel(double x, double y, double dx, double dy, uint32_t mods) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (gSurface && !gSurface.stopped) {
            [gSurface forwardWheelX:x y:y dx:dx dy:dy mods:(NSUInteger)mods];
        }
    });
}

void muqun_macos_key(const char *name, int down, uint32_t mods) {
    NSString *s = MuqunNSString(name);
    dispatch_async(dispatch_get_main_queue(), ^{
        if (gSurface && !gSurface.stopped) {
            [gSurface forwardKey:s down:(down != 0) mods:(NSUInteger)mods];
        }
    });
}

void muqun_macos_close(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (gSurface) {
            gSurface.stopped = YES;
        }
        [NSApp stop:nil];
        NSEvent *wakeup = [NSEvent otherEventWithType:NSEventTypeApplicationDefined
                                             location:NSZeroPoint
                                        modifierFlags:0
                                            timestamp:0
                                         windowNumber:0
                                              context:nil
                                              subtype:0
                                                data1:0
                                                data2:0];
        [NSApp postEvent:wakeup atStart:YES];
    });
}
