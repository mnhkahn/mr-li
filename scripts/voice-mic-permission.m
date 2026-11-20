// Request microphone permission through Apple's public API. Never edits TCC.
#import <AVFoundation/AVFoundation.h>
#import <Foundation/Foundation.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, const char **argv) {
  @autoreleasepool {
    AVAuthorizationStatus status =
        [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
    if (argc == 2 && strcmp(argv[1], "--check") == 0) {
      printf("microphone_authorization=%ld\n", (long)status);
      return 0;
    }
    if (status == AVAuthorizationStatusAuthorized) return 0;
    if (status == AVAuthorizationStatusDenied ||
        status == AVAuthorizationStatusRestricted) {
      fprintf(stderr, "麦克风访问已被系统拒绝。请在本机“安全性与隐私 → 隐私 → 麦克风”中允许启动此程序的终端，然后重新打开启动脚本。\n");
      return 1;
    }
    if (getenv("SSH_CONNECTION")) {
      fprintf(stderr, "请在 Mac mini 桌面双击“启动李老师语音.command”，以便在本机终端显示麦克风授权弹窗。\n");
      return 1;
    }
    fprintf(stderr, "正在向 macOS 申请麦克风权限。请在系统弹窗中点“允许”或“好”；这一步不会录音。\n");
    __block BOOL finished = NO;
    __block BOOL granted = NO;
    [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio
                           completionHandler:^(BOOL allowed) {
      dispatch_async(dispatch_get_main_queue(), ^{
        granted = allowed;
        finished = YES;
      });
    }];
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:180];
    while (!finished && [deadline timeIntervalSinceNow] > 0) {
      [[NSRunLoop currentRunLoop]
          runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.1]];
    }
    if (!finished) {
      fprintf(stderr, "等待系统授权超时。请检查被其他窗口挡住的授权弹窗，再重新打开启动脚本。\n");
      return 1;
    }
    if (!granted) {
      fprintf(stderr, "麦克风未获允许，语音监听没有启动。\n");
      return 1;
    }
    fprintf(stderr, "麦克风已获允许，正在启动李老师语音。\n");
    return 0;
  }
}
