import { Logger } from './logger';
import { environment } from '@env/environment';

export class SharedUtils {
  static appName(): string {
    Logger.log('Getting app name');
    return environment.appName;
  }
}
